//! Wrapper around an SMT Lib 2 compliant solver.
//!
//! The underlying solver runs in a separate process, communication goes
//! through system pipes.

use std::fs::File ;
use std::process::{
  Child, ChildStdin, ChildStdout, Command, Stdio
} ;
use std::io::{ Write, BufWriter, BufReader } ;

use errors::* ;

use common::* ;
use conf::SmtConf ;
use parse::{ IdentParser, ValueParser, ExprParser, RSmtParser } ;





/// Prefix of an actlit identifier.
#[allow(non_upper_case_globals)]
pub static actlit_pref: & str = "|rsmt2 actlit " ;
/// Suffix of an actlit identifier.
#[allow(non_upper_case_globals)]
pub static actlit_suff: & str = "|" ;

/// An activation literal is an opaque wrapper around a `usize`.
///
/// Obtained by a solver's [`get_actlit`][get actlit].
///
/// [get actlit]: ../trait.Solver.html#method.get_actlit
/// (get_actlit documentation)
pub struct Actlit {
  /// ID of the actlit.
  id: usize,
}
impl Actlit {
  /// Unique number of this actlit.
  pub fn uid(& self) -> usize { self.id }
  /// Writes the actlit as an SMT-LIB 2 symbol.
  pub fn write<W>(& self, w: & mut W) -> SmtRes<()> where W: Write {
    write!(w, "{}{}{}", actlit_pref, self.id, actlit_suff) ? ;
    Ok(())
  }
}
impl ::std::ops::Deref for Actlit {
  type Target = usize ;
  fn deref(& self) -> & usize {
    & self.id
  }
}

/// Promise for an asynchronous check-sat.
pub struct FutureCheckSat {
  _nothing: ()
}
fn future_check_sat() -> FutureCheckSat {
  FutureCheckSat { _nothing: () }
}




/// Solver providing most of the SMT-LIB 2.5 commands.
///
/// Note the [`tee` function][tee fun], which takes a file writer to which it
/// will write everything sent to the solver.
///
/// [tee fun]: #method.tee (tee function)
pub struct Solver<Parser> {
  /// Solver configuration.
  conf: SmtConf,
  /// Actual solver process.
  kid: Child,
  /// Solver's stdin.
  stdin: BufWriter<ChildStdin>,
  /// Tee file writer.
  tee: Option< BufWriter<File> >,
  /// Parser on the solver's stdout.
  smt_parser: RSmtParser,
  /// User-defined parser.
  parser: Parser,
  /// Actlit counter.
  actlit: usize,
}

/// Tries to write something in the solver.
macro_rules! wrt {
  ($slf:expr, |$w:ident| $($tail:tt)*) => ({
    if let Some(ref mut $w) = $slf.tee {
      { $($tail)* }
      $w.flush() ?
    }
    let $w = & mut $slf.stdin ;
    { $($tail)* }
    $w.flush() ?
  }) ;
}

impl<Parser> ::std::ops::Drop for Solver<Parser> {
  fn drop(& mut self) {
    let _ = self.kill() ;
    ()
  }
}



/// # Basic functions, not related to SMT-LIB.
impl<Parser> Solver<Parser> {

  /// Spawns the solver kid.
  fn spawn(conf: & SmtConf) -> SmtRes<
    (Child, BufWriter<ChildStdin>, BufReader<ChildStdout>)
  > {
    let mut kid = Command::new(
      // Command.
      conf.get_cmd()
    ).args(
      // Options.
      conf.get_options()
    ).stdin(
      Stdio::piped()
    ).stdout(
      Stdio::piped()
    ).stderr(
      Stdio::piped()
    ).spawn().chain_err::<_, ErrorKind>(
      || format!(
        "While spawning child process with {}",
        conf.get_cmd().to_string()
      ).into()
    ) ? ;

    let mut stdin_opt = None ;
    ::std::mem::swap( & mut stdin_opt, & mut kid.stdin ) ;

    let stdin = if let Some(inner) = stdin_opt {
      BufWriter::new(inner)
    } else {
      bail!("could not retrieve solver's stdin")
    } ;

    let mut stdout_opt = None ;
    ::std::mem::swap( & mut stdout_opt, & mut kid.stdout ) ;

    let stdout = if let Some(inner) = stdout_opt {
      BufReader::new(inner)
    } else {
      bail!("could not retrieve solver's stdout")
    } ;

    Ok((kid, stdin, stdout))
  }

  /// Constructor.
  pub fn new(conf: SmtConf, parser: Parser) -> SmtRes<Self> {

    // Constructing command and spawning kid.
    let (kid, stdin, stdout) = Self::spawn(& conf) ? ;


    let smt_parser = RSmtParser::new(stdout) ;

    let tee = None ;
    let actlit = 0 ;

    Ok(
      Solver {
        kid, stdin, tee, conf, smt_parser, parser, actlit,
      }
    )
  }
  /// Creates a solver kid with the default configuration.
  ///
  /// Mostly used in tests, same as `Self::new( SmtConf::z3(), parser )`.
  pub fn default(parser: Parser) -> SmtRes<Self> {
    Self::new( SmtConf::z3(), parser )
  }
  /// Creates a solver kid with the default z3 configuration.
  ///
  /// Mostly used in tests, same as `Self::new( SmtConf::z3(), parser )`.
  pub fn default_z3(parser: Parser) -> SmtRes<Self> {
    Self::new( SmtConf::z3(), parser )
  }
  /// Creates a solver kid with the default cvc4 configuration.
  ///
  /// Mostly used in tests, same as `Self::new( SmtConf::z3(), parser )`.
  pub fn default_cvc4(parser: Parser) -> SmtRes<Self> {
    Self::new( SmtConf::cvc4(), parser )
  }

  /// Returns the configuration of the solver.
  pub fn conf(& self) -> & SmtConf { & self.conf }

  /// Forces the solver to write all communications to a file.
  ///
  /// Errors if the solver was already tee-ed.
  pub fn tee(& mut self, file: File) -> SmtRes<()> {
    if self.tee.is_some() {
      bail!("Trying to tee a solver that's already tee-ed")
    }
    let mut tee = BufWriter::with_capacity(1000, file) ;
    write!(tee, "; Command:\n; > {}", self.conf.get_cmd()) ? ;
    for option in self.conf.get_options() {
      write!(tee, " {}", option) ?
    }
    write!(tee, "\n\n") ? ;
    self.tee = Some(tee) ;
    Ok(())
  }

  /// Forces the solver to write all communications to a file.
  ///
  /// Opens `file` with `create` and `write`.
  pub fn path_tee<P>(& mut self, path: P) -> SmtRes<()>
  where P: AsRef<::std::path::Path> {
    use std::fs::OpenOptions ;

    let path: & ::std::path::Path = path.as_ref() ;
    let file = OpenOptions::new().create(true).write(true).open(
      path
    ).chain_err(
      || format!("while opening tee file `{}`", path.to_string_lossy())
    ) ? ;
    self.tee(file)
  }

  /// True if the solver is tee-ed.
  pub fn is_teed(& self) -> bool { self.tee.is_some() }

  /// Kills the solver kid.
  ///
  /// The windows version just prints `(exit)` on the kid's `stdin`. Anything
  /// else seems to cause problems.
  ///
  /// This function is automatically called when the solver is dropped.
  #[cfg(windows)]
  pub fn kill(& mut self) -> SmtRes<()> {
    let _ = writeln!(self.stdin, "(exit)") ;
    let _ = self.stdin.flush() ;
    Ok(())
  }
  /// Kills the solver kid.
  ///
  /// The windows version just prints `(exit)` on the kid's `stdin`. Anything
  /// else seems to cause problems.
  ///
  /// This function is automatically called when the solver is dropped.
  #[cfg(not(windows))]
  pub fn kill(& mut self) -> SmtRes<()> {
    let _ = writeln!(self.stdin, "(exit)") ;
    let _ = self.stdin.flush() ;
    if let None = self.kid.try_wait().chain_err(
      || "waiting for child process to exit"
    ) ? {
      self.kid.kill().chain_err::<_, ErrorKind>(
        || "while killing child process".into()
      ) ?
    }
    Ok(())
  }




  /// Internal comment function.
  #[inline]
  fn cmt(file: & mut BufWriter<File>, blah: & str) -> SmtRes<()> {
    for line in blah.lines() {
      write!(file, "; {}\n", line) ?
    }
    file.flush() ? ;
    Ok(())
  }


  /// Prints a comment in the tee-ed file if any.
  #[inline]
  pub fn comment_args(& mut self, args: ::std::fmt::Arguments) -> SmtRes<()> {
    if let Some(ref mut file) = self.tee {
      Self::cmt(file, & format!("{}", args)) ?
    }
    Ok(())
  }

  /// Prints a comment in the tee-ed file if any. String version.
  #[inline]
  pub fn comment(& mut self, blah: & str) -> SmtRes<()> {
    if let Some(ref mut file) = self.tee {
      Self::cmt(file, blah) ?
    }
    Ok(())
  }

}



/// # Basic SMT-LIB parser-agnostic commands.
impl<Parser> Solver<Parser> {


  /// Asserts an expression.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.assert("(= 0 1)").unwrap() ;
  /// ```
  #[inline]
  pub fn assert<Expr>(
    & mut self, expr: & Expr
  ) -> SmtRes<()>
  where Expr: ?Sized + Expr2Smt<()> {
    self.assert_with(expr, ())
  }

  /// Check-sat command, turns `unknown` results into errors.
  ///
  /// # See also
  ///
  /// - [`print_check_sat`][print]
  /// - [`parse_check_sat`][get]
  ///
  /// If you want a more natural way to handle unknown results, see
  /// `parse_check_sat_or_unk`.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// # let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_const("x", "Int").unwrap() ;
  /// solver.declare_const("y", "Int").unwrap() ;
  ///
  /// solver.push(1).unwrap() ;
  /// solver.assert("(= (+ x y) 0)").unwrap() ;
  /// let maybe_sat = solver.check_sat().unwrap() ;
  /// assert_eq! { maybe_sat, true }
  /// solver.pop(1).unwrap() ;
  ///
  /// solver.assert("(= (+ (* x x y) (* y y x)) 7654321)").unwrap() ;
  /// let sat_res = & solver.check_sat() ;
  ///
  /// match * sat_res {
  ///   Err(ref e) => match * e.kind() {
  ///     ::rsmt2::errors::ErrorKind::Unknown => (),
  ///     _ => panic!("expected unknown"),
  ///   },
  ///   Ok(res) => panic!("expected error: {:?}", res),
  /// }
  /// ```
  ///
  /// [print]: #method.print_check_sat
  /// (print_check_sat function)
  /// [get]: #method.parse_check_sat
  /// (parse_check_sat function)
  pub fn check_sat(& mut self) -> SmtRes<bool> {
    let future = self.print_check_sat() ? ;
    self.parse_check_sat(future)
  }

  /// Check-sat command, turns `unknown` results in `None`.
  ///
  /// # See also
  ///
  /// - [`parse_check_sat_or_unk`][get]
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// # let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_const("x", "Int").unwrap() ;
  /// solver.declare_const("y", "Int").unwrap() ;
  ///
  /// solver.push(1).unwrap() ;
  /// solver.assert("(= (+ x y) 0)").unwrap() ;
  /// let maybe_sat = solver.check_sat_or_unk().unwrap() ;
  /// assert_eq! { maybe_sat, Some(true) }
  /// solver.pop(1).unwrap() ;
  ///
  /// solver.assert("(= (+ (* x x y) (* y y x)) 7654321)").unwrap() ;
  /// let maybe_sat = solver.check_sat_or_unk().unwrap() ;
  /// // Unknown ~~~~~~~~~~~~~vvvv
  /// assert_eq! { maybe_sat, None }
  /// ```
  ///
  /// [get]: #method.parse_check_sat_or_unk
  /// (parse_check_sat_or_unk function)
  pub fn check_sat_or_unk(& mut self) -> SmtRes< Option<bool> > {
    let future = self.print_check_sat() ? ;
    self.parse_check_sat_or_unk(future)
  }



  /// Resets the underlying solver. Restarts the kid process if no reset
  /// command is supported.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.assert("(= 0 1)").unwrap() ;
  /// assert! { ! solver.check_sat().unwrap() }
  /// solver.reset().unwrap() ;
  /// assert! { solver.check_sat().unwrap() }
  /// ```
  #[inline]
  pub fn reset(
    & mut self
  ) -> SmtRes<()> {
    self.actlit = 0 ;
    wrt! {
      self, |w| write_str(w, "(reset)\n") ?
    }
    Ok(())
  }


  /// Declares a new constant.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_const("x", "Int").unwrap()
  /// ```
  #[inline]
  pub fn declare_const<Sym, Sort> (
    & mut self, symbol: & Sym, out_sort: & Sort
  ) -> SmtRes<()>
  where Sym: ?Sized + Sym2Smt<()>, Sort: ?Sized + Sort2Smt {
    self.declare_const_with(symbol, out_sort, ())
  }

  /// Declares a new function symbol.
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_fun(
  ///   "my_symbol", & [ "Int", "Real", "Bool" ], "Bool"
  /// ).unwrap()
  /// ```
  #[inline]
  pub fn declare_fun<'a, FunSym, ArgSort, Args, OutSort> (
    & mut self, symbol: & FunSym, args: Args, out: & OutSort
  ) -> SmtRes<()>
  where
  FunSym: ?Sized + Sym2Smt<()>,
  ArgSort: ?Sized + Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  Args: Copy + IntoIterator< Item = & 'a ArgSort > {
    self.declare_fun_with(symbol, args, out, ())
  }

  /// Defines a new constant function symbol.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.define_const(
  ///   "seven", "Int", "7"
  /// ).unwrap()
  /// ```
  #[inline]
  pub fn define_const<
    'a, FunSym, OutSort, Body
  >(
    & mut self, symbol: & FunSym, out: & OutSort, body: & Body
  ) -> SmtRes<()>
  where
  OutSort: ?Sized + Sort2Smt,
  FunSym: ?Sized + Sym2Smt<()>,
  Body: ?Sized + Expr2Smt<()> {
    self.define_const_with(symbol, out, body, ())
  }

  /// Defines a new function symbol.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.define_fun(
  ///   "abs", & [ ("n", "Int") ], "Int", "(ite (< x 0) (- x) x)"
  /// ).unwrap()
  /// ```
  #[inline]
  pub fn define_fun<
    'a, FunSym, ArgSym, ArgSort, Args, OutSort, Body
  >(
    & mut self, symbol: & FunSym, args: Args, out: & OutSort, body: & Body
  ) -> SmtRes<()>
  where
  ArgSort: Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  FunSym: ?Sized + Sym2Smt<()>,
  ArgSym: Sym2Smt<()> + 'a,
  Body: ?Sized + Expr2Smt<()>,
  Args: Copy + IntoIterator< Item = & 'a (ArgSym, ArgSort) > {
    self.define_fun_with(symbol, args, out, body, ())
  }

  /// Pushes `n` layers on the assertion stack.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_const("x", "Int").unwrap() ;
  /// solver.declare_const("y", "Int").unwrap() ;
  /// solver.assert("(= x y)").unwrap() ;
  /// let sat = solver.check_sat().unwrap() ;
  /// assert!(sat) ;
  ///
  /// solver.push(1).unwrap() ;
  /// solver.assert("(= x (+ x 1))").unwrap() ;
  /// let sat = solver.check_sat().unwrap() ;
  /// assert!(! sat) ;
  /// solver.pop(1).unwrap() ;
  ///
  /// let sat = solver.check_sat().unwrap() ;
  /// assert!(sat) ;
  /// ```
  #[inline]
  pub fn push(& mut self, n: u8) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(push {})\n", n) ?
    }
    Ok(())
  }

  /// Pops `n` layers off the assertion stack.
  ///
  /// See [`push`][push] for examples.
  ///
  /// [push]: #method.push
  /// (push function for solver)
  #[inline]
  pub fn pop(& mut self, n: u8) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(pop {})\n", n) ?
    }
    Ok(())
  }

  /// Check-sat assuming command, turns `unknown` results into errors.
  pub fn check_sat_assuming<'a, Ident, Idents>(
    & mut self, idents: Idents
  ) -> SmtRes<bool>
  where
  Ident: ?Sized + Sym2Smt<()> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    self.check_sat_assuming_with(idents, ())
  }

  /// Check-sat assuming command, turns `unknown` results into `None`.
  pub fn check_sat_assuming_or_unk<'a, Ident, Idents>(
    & mut self, idents: Idents
  ) -> SmtRes<Option<bool>>
  where
  Ident: ?Sized + Sym2Smt<()> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    self.check_sat_assuming_or_unk_with(idents, ())
  }



  /// Sets the logic.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.set_logic( ::rsmt2::Logic::QF_UF ).unwrap() ;
  /// ```
  #[inline]
  pub fn set_logic(
    & mut self, logic: Logic
  ) -> SmtRes<()> {
    wrt! {
      self, |w| {
        write_str(w, "(set-logic ") ? ;
        logic.to_smt2(w, ()) ? ;
        write_str(w, ")\n") ?
      }
    }
    Ok(())
  }


  /// Defines a recursive function.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.define_fun_rec(
  ///   "abs", & [ ("n", "Int") ], "Int", "(ite (< x 0) (abs (- x)) x)"
  /// ).unwrap()
  /// ```
  #[inline]
  pub fn define_fun_rec<
    'a, FunSym, ArgSym, ArgSort, Args, OutSort, Body
  >(
    & mut self,  symbol: & FunSym, args: Args, out: & OutSort, body: & Body
  ) -> SmtRes<()>
  where
  ArgSort: Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  FunSym: ?Sized + Sym2Smt<()>,
  ArgSym: Sym2Smt<()> + 'a,
  Body: ?Sized + Expr2Smt<()>,
  Args: Copy + IntoIterator< Item = & 'a (ArgSym, ArgSort) > {
    self.define_fun_rec_with(symbol, args, out, body, ())
  }




  /// Defines some (possibily mutually) recursive functions.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.define_funs_rec( & [
  ///   ("abs_1", & [ ("n", "Int") ], "Int", "(ite (< x 0) (abs_2 (- x)) x)"),
  ///   ("abs_2", & [ ("n", "Int") ], "Int", "(ite (< x 0) (abs_3 (- x)) x)"),
  ///   ("abs_3", & [ ("n", "Int") ], "Int", "(ite (< x 0) (abs_1 (- x)) x)"),
  /// ] ).unwrap()
  /// ```
  #[inline]
  pub fn define_funs_rec<
    'a, FunSym, ArgSym, ArgSort, Args, ArgsRef, OutSort, Body, Funs
  >(
    & mut self, funs: Funs,
  ) -> SmtRes<()>
  where
  FunSym: Sym2Smt<()> + 'a,
  ArgSym: Sym2Smt<()> + 'a,
  ArgSort: Sort2Smt + 'a,
  OutSort: Sort2Smt + 'a,
  Body: Expr2Smt<()> + 'a,
  & 'a Args: IntoIterator< Item = & 'a (ArgSym, ArgSort) > + 'a,
  Args: ?Sized,
  ArgsRef: 'a + AsRef<Args>,
  Funs: Copy + IntoIterator< Item = & 'a (FunSym, ArgsRef, OutSort, Body) > {
    self.define_funs_rec_with(funs, ())
  }


  /// Declares mutually recursive datatypes.
  ///
  /// A relatively recent version of z3 is recommended.
  ///
  /// # Examples
  ///
  /// ```norun
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_datatypes( & [
  ///   ( "Tree", 1, ["T"],
  ///     [ "leaf", "(node (value T) (children (TreeList T)))" ] ),
  ///   ( "TreeList", 1, ["T"],
  ///     [ "nil", "(cons (car (Tree T)) (cdr (TreeList T)))" ]),
  /// ] ).unwrap() ;
  ///
  /// solver.declare_const( "t1", "(Tree Int)" ).unwrap() ;
  /// solver.declare_const( "t2", "(Tree Bool)" ).unwrap() ;
  ///
  /// solver.assert("(> (value t1) 20)").unwrap() ;
  /// solver.assert("(not (is-leaf t2))").unwrap() ;
  /// solver.assert("(not (value t2))").unwrap() ;
  ///
  /// let sat = solver.check_sat().unwrap() ;
  /// assert! { sat } panic! { "aaa" }
  /// ```
  pub fn declare_datatypes<
    'a, Sort, Param, ParamList, Def, DefList, All
  >(
    & mut self, defs: All,
  ) -> SmtRes<()>
  where
  Sort: Sort2Smt + 'a,

  Param: Sort2Smt + 'a,
  & 'a ParamList: IntoIterator< Item = & 'a Param > + 'a,

  Def: Sort2Smt + 'a,
  & 'a DefList: IntoIterator<
    Item = & 'a Def
  > + 'a,
  All: Copy + IntoIterator<
    Item = & 'a (Sort, usize, ParamList, DefList)
  > + 'a {

    wrt! {
      self, |w| {
        write!(w, "(declare-datatypes (") ? ;
        for & (ref sort, arity, _, _) in defs {
          write!(w, " (") ? ;
          sort.sort_to_smt2(w) ? ;
          write!(w, " {})", arity) ? ;
        }
        write!(w, " ) (") ? ;

        for & (_, arity, ref params, ref defs) in defs {
          write!(w, " (") ? ;

          if arity > 0 {
            write!(w, "par (") ? ;
            for param in params {
              write!(w, " ") ? ;
              param.sort_to_smt2(w) ? ;
            }
            write!(w, " ) (") ?
          }

          for def in defs {
            write!(w, " ") ? ;
            def.sort_to_smt2(w) ? ;
          }

          write!(w, " )") ? ;

          if arity > 0 {
            write!(w, " )") ?
          }
        }

        write!(w, " ) )\n") ? ;
      }
    }

    Ok(())

  }


}



/// # Basic SMT-LIB commands using the user's parser.
impl<Parser: Copy> Solver<Parser> {

  /// Get-model command.
  pub fn get_model<Ident, Type, Value>(
    & mut self
  ) -> SmtRes<Vec<(Ident, Vec<Type>, Type, Value)>>
  where
  Parser: for<'a> IdentParser<Ident, Type, & 'a mut RSmtParser> +
          for<'a> ValueParser<Value, & 'a mut RSmtParser> {
    self.print_get_model() ? ;
    self.parse_get_model()
  }

  /// Get-model command when all the symbols are nullary.
  pub fn get_model_const<Ident, Type, Value>(
    & mut self
  ) -> SmtRes<Vec<(Ident, Type, Value)>>
  where
  Parser: for<'a> IdentParser<Ident, Type, & 'a mut RSmtParser> +
          for<'a> ValueParser<Value, & 'a mut RSmtParser> {
    self.print_get_model() ? ;
    self.parse_get_model_const()
  }

  /// Get-values command.
  ///
  /// Notice that the input expression type and the output one have no reason
  /// to be the same.
  pub fn get_values<'a, Expr, Exprs, PExpr, PValue>(
    & mut self, exprs: Exprs
  ) -> SmtRes<Vec<(PExpr, PValue)>>
  where
  Parser: for<'b> ExprParser<PExpr, (), & 'b mut RSmtParser> +
          for<'b> ValueParser<PValue, & 'b mut RSmtParser>,
  Expr: ?Sized + Expr2Smt<()> + 'a,
  Exprs: Copy + IntoIterator< Item = & 'a Expr > {
    self.get_values_with(exprs, ())
  }

}




/// # Actlit-related functions.
///
/// See the [`actlit`'s module doc][actlits] for more details.
///
/// [actlits]: actlit/index.html
/// (actlit module documentation)
impl<Parser> Solver<Parser> {

  // |===| Actlit stuff.

  /// True if no actlits have been created since the last reset.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  fn has_actlits(& self) -> bool { self.actlit > 0 }

  /// Introduces a new actlit.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn get_actlit(& mut self) -> SmtRes<Actlit> {
    let id = self.actlit ;
    self.actlit += 1 ;
    let next_actlit = Actlit { id } ;
    wrt! {
      self, |w| write!(
        w, "(declare-fun {}{}{} () Bool)\n",
        actlit_pref, next_actlit.id, actlit_suff
      ) ?
    }
    Ok(next_actlit)
  }

  /// Deactivates an activation literal, alias for
  /// `solver.set_actlit(actlit, false)`.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn de_actlit(& mut self, actlit: Actlit) -> SmtRes<()> {
    self.set_actlit(actlit, false)
  }

  /// Forces the value of an actlit and consumes it.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn set_actlit(
    & mut self, actlit: Actlit, b: bool
  ) -> SmtRes<()> {
    wrt! {
      self, |w| {
        if b {
          write!(w, "(assert ") ?
        } else {
          write!(w, "(assert (not ") ?
        }
        actlit.write(w) ? ;
        if b {
          write!(w, ")\n") ?
        } else {
          write!(w, ") )\n") ?
        }
      }
    }
    Ok(())
  }

  /// Asserts an expression without print information, guarded by an activation
  /// literal.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn assert_act<Expr>(
    & mut self, actlit: & Actlit, expr: & Expr
  ) -> SmtRes<()>
  where Expr: ?Sized + Expr2Smt<()> {
    self.assert_act_with(actlit, expr, ())
  }

  /// Check-sat command with activation literals, turns `unknown` results into
  /// errors.
  pub fn check_sat_act<'a, Actlits>(
    & mut self, actlits: Actlits
  ) -> SmtRes<bool>
  where
  Actlits: Copy + IntoIterator<Item = & 'a Actlit> {
    let future = self.print_check_sat_act(actlits) ? ;
    self.parse_check_sat(future)
  }

  /// Check-sat command with activation literals, turns `unknown` results in
  /// `None`.
  pub fn check_sat_act_or_unk<'a, Actlits>(
    & mut self, actlits: Actlits
  ) -> SmtRes< Option<bool> >
  where
  Actlits: Copy + IntoIterator<Item = & 'a Actlit> {
    let future = self.print_check_sat_act(actlits) ? ;
    self.parse_check_sat_or_unk(future)
  }
}





/// # SMT-LIB asynchronous commands.
impl<Parser> Solver<Parser> {

  /// Prints a check-sat command.
  ///
  /// Allows to print the `check-sat` and get the result later, *e.g.* with
  /// [`parse_check_sat`][parse]. 
  ///
  /// # Examples
  ///
  /// ```
  /// use rsmt2::Solver ;
  ///
  /// let mut solver = Solver::default(()).unwrap() ;
  ///
  /// solver.declare_const("x", "Int").unwrap() ;
  /// solver.declare_const("y", "Int").unwrap() ;
  ///
  /// solver.assert("(= (+ x y) 0)").unwrap() ;
  ///
  /// let future = solver.print_check_sat().unwrap() ;
  /// // Do stuff while the solver works.
  /// let sat = solver.parse_check_sat(future).unwrap() ;
  /// assert! { sat }
  /// ```
  ///
  /// [parse]: #method.parse_check_sat
  /// (parse_check_sat function for Solver)
  #[inline]
  pub fn print_check_sat(& mut self) -> SmtRes<FutureCheckSat> {
    wrt! {
      self, |w| write_str(w, "(check-sat)\n") ?
    }
    Ok(future_check_sat())
  }

  /// Check-sat command, with actlits.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn print_check_sat_act<'a, Actlits>(
    & mut self, actlits: Actlits
  ) -> SmtRes<FutureCheckSat>
  where
  Actlits: Copy + IntoIterator<Item = & 'a Actlit> {
    wrt! {
      self, |w| {
        write_str(w, "(check-sat-assuming (") ? ;
        for actlit in actlits {
          write!(w, " ") ? ;
          actlit.write(w) ?
        }
        write_str(w, ") )\n") ?
      }
    }
    Ok(future_check_sat())
  }


  /// Parse the result of a check-sat, turns `unknown` results into errors.
  #[inline]
  pub fn parse_check_sat(
    & mut self, _: FutureCheckSat
  ) -> SmtRes<bool> {
    if let Some(res) = self.smt_parser.check_sat() ? {
      Ok(res)
    } else {
      Err( ErrorKind::Unknown.into() )
    }
  }

  /// Parse the result of a check-sat, turns `unknown` results into `None`.
  #[inline]
  pub fn parse_check_sat_or_unk(
    & mut self, _: FutureCheckSat
  ) -> SmtRes< Option<bool> > {
    self.smt_parser.check_sat()
  }



  /// Get-model command.
  #[inline]
  fn print_get_model(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-model)\n") ?
    }
    Ok(())
  }

  /// Get-assertions command.
  #[allow(dead_code)]
  fn print_get_assertions(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-assertions)\n") ?
    }
    Ok(())
  }
  /// Get-assignment command.
  #[allow(dead_code)]
  fn print_get_assignment(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-assignment)\n") ?
    }
    Ok(())
  }
  /// Get-unsat-assumptions command.
  #[allow(dead_code)]
  fn print_get_unsat_assumptions(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-unsat-assumptions)\n") ?
    }
    Ok(())
  }
  /// Get-proof command.
  #[allow(dead_code)]
  fn print_get_proof(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-proof)\n") ?
    }
    Ok(())
  }
  /// Get-unsat-core command.
  #[allow(dead_code)]
  fn print_get_unsat_core(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(get-unsat-core)\n") ?
    }
    Ok(())
  }

  /// Get-values command.
  fn print_get_values_with<'a, Info, Expr, Exprs>(
    & mut self, exprs: Exprs, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  Expr: ?Sized + Expr2Smt< Info > + 'a,
  Exprs: Clone + IntoIterator< Item = & 'a Expr > {
    wrt! {
      self, |w| {
        write!(w, "(get-value (") ? ;
        for e in exprs.clone() {
          write_str(w, "\n  ") ? ;
          e.expr_to_smt2(w, info) ?
        }
        write_str(w, "\n) )\n") ?
      }
    }
    Ok(())
  }

  /// Check-sat with assumptions command with unit info.
  pub fn print_check_sat_assuming<'a, Ident, Idents>(
    & mut self, bool_vars: Idents
  ) -> SmtRes<FutureCheckSat>
  where
  Ident: ?Sized + Sym2Smt<()> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    self.print_check_sat_assuming_with(bool_vars, ())
  }

  /// Check-sat with assumptions command.
  pub fn print_check_sat_assuming_with<'a, Info, Ident, Idents>(
    & mut self, bool_vars: Idents, info: Info
  ) -> SmtRes<FutureCheckSat>
  where
  Info: Copy,
  Ident: ?Sized + Sym2Smt<Info> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    match self.conf.get_check_sat_assuming() {
      Some(ref cmd) => {
        wrt! {
          self, |w| {
            write!(w, "({}\n ", cmd) ? ;
            for sym in bool_vars {
              write_str(w, " ") ? ;
              sym.sym_to_smt2(w, info) ?
            } ;
            write_str(w, "\n)\n") ?
          }
        }
        Ok(future_check_sat())
      },
      _ => Err(
        format!(
          "{} does not support check-sat-assuming", self.conf.desc()
        ).into()
      ),
    }
  }

  /// Parse the result of a get-model.
  fn parse_get_model<Ident, Type, Value>(
    & mut self
  ) -> SmtRes<Vec<(Ident, Vec<Type>, Type, Value)>>
  where
  Parser: for<'a> IdentParser<Ident, Type, & 'a mut RSmtParser> +
          for<'a> ValueParser<Value, & 'a mut RSmtParser> {
    let has_actlits = self.has_actlits() ;
    let res = self.smt_parser.get_model(has_actlits, self.parser) ;
    if res.is_err() && self.conf.get_models() {
      res.chain_err(
        || "\
          Note: model production is not active \
          for this SmtConf (`conf.models()`)\
        "
      )
    } else {
      res
    }
  }

  /// Parse the result of a get-model where all the symbols are nullary.
  fn parse_get_model_const<Ident, Type, Value>(
    & mut self
  ) -> SmtRes<Vec<(Ident, Type, Value)>>
  where
  Parser: for<'a> IdentParser<Ident, Type, & 'a mut RSmtParser> +
          for<'a> ValueParser<Value, & 'a mut RSmtParser> {
    let has_actlits = self.has_actlits() ;
    let res = self.smt_parser.get_model_const(has_actlits, self.parser) ;
    if res.is_err() && self.conf.get_models() {
      res.chain_err(
        || "\
          Note: model production is not active \
          for this SmtConf (`conf.models()`)\
        "
      )
    } else {
      res
    }
  }

  /// Parse the result of a get-values.
  fn parse_get_values_with<Info, Expr, Value>(
    & mut self, info: Info
  ) -> SmtRes<Vec<(Expr, Value)>>
  where
  Info: Copy,
  Parser: for<'a> ExprParser<Expr, Info, & 'a mut RSmtParser> +
          for<'a> ValueParser<Value, & 'a mut RSmtParser> {
    let res = self.smt_parser.get_values(self.parser, info) ;
    if res.is_err() && self.conf.get_models() {
      res.chain_err(
        || "\
          Note: model production is not active \
          for this SmtConf (`conf.models()`)\
        "
      )
    } else {
      res
    }
  }

  /// Get-values command.
  ///
  /// Notice that the input expression type and the output one have no reason
  /// to be the same.
  fn get_values_with<
    'a, Info, Expr, Exprs, PExpr, PValue
  >(
    & mut self, exprs: Exprs, info: Info
  ) -> SmtRes<Vec<(PExpr, PValue)>>
  where
  Info: Copy,
  Parser: for<'b> ExprParser<PExpr, Info, & 'b mut RSmtParser> +
          for<'b> ValueParser<PValue, & 'b mut RSmtParser>,
  Expr: ?Sized + Expr2Smt<Info> + 'a,
  Exprs: Copy + IntoIterator< Item = & 'a Expr > {
    self.print_get_values_with( exprs, info.clone() ) ? ;
    self.parse_get_values_with(info)
  }

}







/// # Sort-related SMT-LIB commands.
impl<Parser> Solver<Parser> {

  /// Declares a new sort.
  ///
  /// # Examples
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.declare_sort("A", 0).unwrap() ;
  /// solver.declare_const("x", "A").unwrap() ;
  /// solver.declare_const("y", "A").unwrap() ;
  /// solver.declare_fun("f", & [ "A" ], "A").unwrap() ;
  /// solver.assert("(= (f (f x)) x)").unwrap() ;
  /// solver.assert("(= (f x) y)").unwrap() ;
  /// solver.assert("(not (= x y))").unwrap() ;
  /// let sat = solver.check_sat().unwrap() ;
  /// ```
  #[inline]
  pub fn declare_sort<Sort>(
    & mut self, sort: & Sort, arity: u8
  ) -> SmtRes<()>
  where Sort: ?Sized + Sort2Smt {
    wrt! {
      self, |w| {
        write_str(w, "(declare-sort ") ? ;
        sort.sort_to_smt2(w) ? ;
        write!(w, " {})\n", arity) ?
      }
    }
    Ok(())
  }

  /// Defines a new sort.
  ///
  /// # Examples
  ///
  /// Note the use of [`define_null_sort`][def] to avoid problems with empty
  /// arguments.
  ///
  /// ```
  /// # use rsmt2::Solver ;
  /// let mut solver = Solver::default(()).unwrap() ;
  /// solver.define_sort("Set", & ["T"], "(Array T Bool)").unwrap() ;
  /// solver.define_null_sort("IList", "(List Int)").unwrap() ;
  /// solver.define_sort(
  ///   "List-Set", & ["T"], "(Array (List T) Bool)"
  /// ).unwrap() ;
  /// solver.define_null_sort("I", "Int").unwrap() ;
  ///
  /// solver.declare_const("s1", "(Set I)").unwrap() ;
  /// solver.declare_const("s2", "(List-Set Int)").unwrap() ;
  /// solver.declare_const("a", "I").unwrap() ;
  /// solver.declare_const("l", "IList").unwrap() ;
  ///
  /// solver.assert("(= (select s1 a) true)").unwrap() ;
  /// solver.assert("(= (select s2 l) false)").unwrap() ;
  /// let sat = solver.check_sat().unwrap() ;
  /// ```
  ///
  /// [def]: #method.define_null_sort
  /// (define_null_sort function)
  #[inline]
  pub fn define_sort<'a, Sort, Arg, Args, Body>(
    & mut self, sort: & Sort, args: Args, body: & Body
  ) -> SmtRes<()>
  where
  Sort: ?Sized + Sort2Smt,
  Arg: ?Sized + Sort2Smt + 'a,
  Body: ?Sized + Sort2Smt,
  Args: Copy + IntoIterator< Item = & 'a Arg > {
    wrt!{
      self, |w| {
        write_str(w, "( define-sort ") ? ;
        sort.sort_to_smt2(w) ? ;
        write_str(w, "\n   ( ") ? ;
        for arg in args {
          arg.sort_to_smt2(w) ? ;
          write_str(w, " ") ?
        }
        write_str(w, ")\n   ") ? ;
        body.sort_to_smt2(w) ? ;
        write_str(w, "\n)\n") ?
      }
    }
    Ok(())
  }

  /// Defines a new nullary sort.
  ///
  /// When using [`define_sort`][def], rust complains because it does not know
  /// what the `Arg` type parameter is, since the `args` parameter is empty. So
  /// this function can be useful.
  ///
  /// This could be fixed with a default type for `Arg`, like `Body` for
  /// instance, but this is currently not possible in a function.
  ///
  /// [def]: #method.define_sort
  /// (define_sort function)
  #[inline]
  pub fn define_null_sort<'a, Sort, Body>(
    & mut self, sort: & Sort, body: & Body
  ) -> SmtRes<()>
  where
  Sort: ?Sized + Sort2Smt,
  Body: ?Sized + Sort2Smt {
    self.define_sort::<Sort, Body, Option<& Body>, Body>(sort, None, body)
  }

}


/// # SMT-LIB commands vehiculating user information.
impl<Parser> Solver<Parser> {

  /// Declares a new constant.
  #[inline]
  pub fn declare_const_with<Info, Sym, Sort> (
    & mut self, symbol: & Sym, out_sort: & Sort, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  Sym: ?Sized + Sym2Smt<Info>,
  Sort: ?Sized + Sort2Smt {
    wrt! {
      self, |w| {
        write_str(w, "(declare-const ") ? ;
        symbol.sym_to_smt2(w, info) ? ;
        write_str(w, " ") ? ;
        out_sort.sort_to_smt2(w) ? ;
        write_str(w, ")\n") ?
      }
    }
    Ok(())
  }

  /// Declares a new function symbol.
  #[inline]
  pub fn declare_fun_with<'a, Info, FunSym, ArgSort, Args, OutSort> (
    & mut self, symbol: & FunSym, args: Args, out: & OutSort, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  FunSym: ?Sized + Sym2Smt<Info>,
  ArgSort: ?Sized + Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  Args: Copy + IntoIterator< Item = & 'a ArgSort > {
    wrt! {
      self, |w| {
        write_str(w, "(declare-fun ") ? ;
        symbol.sym_to_smt2(w, info) ? ;
        write_str(w, " ( ") ? ;
        for arg in args.into_iter() {
          arg.sort_to_smt2(w) ? ;
          write_str(w, " ") ?
        }
        write_str(w, ") ") ? ;
        out.sort_to_smt2(w) ? ;
        write_str(w, ")\n") ?
      }
    }
    Ok(())
  }

  /// Defines a new constant.
  #[inline]
  pub fn define_const_with<Info, Sym, Sort, Body> (
    & mut self, symbol: & Sym, out_sort: & Sort, body: & Body, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  Sym: ?Sized + Sym2Smt<Info>,
  Sort: ?Sized + Sort2Smt,
  Body: ?Sized + Expr2Smt<Info> {
    wrt! {
      self, |w| {
        write_str(w, "(define-const ") ? ;
        symbol.sym_to_smt2(w, info) ? ;
        write_str(w, " ") ? ;
        out_sort.sort_to_smt2(w) ? ;
        write_str(w, " ") ? ;
        body.expr_to_smt2(w, info) ? ;
        write_str(w, ")\n") ?
      }
    }
    Ok(())
  }

  /// Defines a new function symbol.
  #[inline]
  pub fn define_fun_with<
    'a, Info, FunSym, ArgSym, ArgSort, Args, OutSort, Body
  >(
    & mut self, symbol: & FunSym, args: Args,
    out: & OutSort, body: & Body, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  ArgSort: Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  FunSym: ?Sized + Sym2Smt<Info>,
  ArgSym: Sym2Smt<Info> + 'a,
  Body: ?Sized + Expr2Smt<Info>,
  Args: Copy + IntoIterator< Item = & 'a (ArgSym, ArgSort) > {
    wrt! {
      self, |w| {
        write_str(w, "(define-fun ") ? ;
        symbol.sym_to_smt2(w, info) ? ;
        write_str(w, " ( ") ? ;
        for arg in args {
          let (ref sym, ref sort) = * arg ;
          write_str(w, "(") ? ;
          sym.sym_to_smt2(w, info) ? ;
          write_str(w, " ") ? ;
          sort.sort_to_smt2(w) ? ;
          write_str(w, ") ") ?
        }
        write_str(w, ") ") ? ;
        out.sort_to_smt2(w) ? ;
        write_str(w, "\n   ") ? ;
        body.expr_to_smt2(w, info) ? ;
        write_str(w, "\n)\n") ?
      }
    }
    Ok(())
  }

  /// Defines some new (possibily mutually) recursive functions.
  #[inline]
  pub fn define_funs_rec_with<
    'a, Info, FunSym, ArgSym, ArgSort, Args, ArgsRef, OutSort, Body, Funs
  >(
    & mut self, funs: Funs, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  FunSym: Sym2Smt<Info> + 'a,
  ArgSym: Sym2Smt<Info> + 'a,
  ArgSort: Sort2Smt + 'a,
  OutSort: Sort2Smt + 'a,
  Body: Expr2Smt<Info> + 'a,
  & 'a Args: IntoIterator< Item = & 'a (ArgSym, ArgSort) > + 'a,
  Args: ?Sized,
  ArgsRef: 'a + AsRef<Args>,
  Funs: Copy + IntoIterator< Item = & 'a (FunSym, ArgsRef, OutSort, Body) > {
    wrt! {
      self, |w| {
        // Header.
        write_str(w, "(define-funs-rec (\n") ? ;

        // Signatures.
        for fun in funs {
          let (ref sym, ref args, ref out, _) = * fun ;
          write_str(w, "   (") ? ;
          sym.sym_to_smt2(w, info) ? ;
          write_str(w, " ( ") ? ;
          for arg in args.as_ref() {
            let (ref sym, ref sort) = * arg ;
            write_str(w, "(") ? ;
            sym.sym_to_smt2(w, info) ? ;
            write_str(w, " ") ? ;
            sort.sort_to_smt2(w) ? ;
            write_str(w, ") ") ?
          }
          write_str(w, ") ") ? ;
          out.sort_to_smt2(w) ? ;
          write_str(w, ")\n") ?
        }
        write_str(w, " ) (") ? ;

        // Bodies
        for fun in funs {
          let (_, _, _, ref body) = * fun ;
          write_str(w, "\n   ") ? ;
          body.expr_to_smt2(w, info) ?
        }
        write_str(w, "\n )\n)\n") ?
      }
    }
    Ok(())
  }

  /// Defines a new recursive function.
  #[inline]
  pub fn define_fun_rec_with<
    'a, Info, FunSym, ArgSym, ArgSort, Args, OutSort, Body
  >(
    & mut self,  symbol: & FunSym, args: Args,
    out: & OutSort, body: & Body, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  ArgSort: Sort2Smt + 'a,
  OutSort: ?Sized + Sort2Smt,
  FunSym: ?Sized + Sym2Smt<Info>,
  ArgSym: Sym2Smt<Info> + 'a,
  Body: ?Sized + Expr2Smt<Info>,
  Args: Copy + IntoIterator< Item = & 'a (ArgSym, ArgSort) > {
    wrt! {
      self, |w| {
        // Header.
        write_str(w, "(define-fun-rec (\n") ? ;

        // Signature.
        write_str(w, "   (") ? ;
        symbol.sym_to_smt2(w, info) ? ;
        write_str(w, " ( ") ? ;
        for arg in args {
          let (ref sym, ref sort) = * arg ;
          write_str(w, "(") ? ;
          sym.sym_to_smt2(w, info) ? ;
          write_str(w, " ") ? ;
          sort.sort_to_smt2(w) ? ;
          write_str(w, ") ") ?
        }
        write_str(w, ") ") ? ;
        out.sort_to_smt2(w) ? ;
        write_str(w, ")\n") ? ;
        write_str(w, " ) (") ? ;

        // Body.
        write_str(w, "\n   ") ? ;
        body.expr_to_smt2(w, info) ? ;
        write_str(w, "\n )\n)\n") ?
      }
    }
    Ok(())
  }

  /// Asserts an expression with some print information, guarded by an
  /// activation literal.
  ///
  /// See the [`actlit`'s module doc][actlits] for more details.
  ///
  /// [actlits]: actlit/index.html
  /// (actlit module documentation)
  #[inline]
  pub fn assert_act_with<Info, Expr>(
    & mut self, actlit: & Actlit, expr: & Expr, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  Expr: ?Sized + Expr2Smt<Info>, {
    wrt! {
      self, |w| {
        write_str(w, "(assert\n  (=>\n    ") ? ;
        actlit.write(w) ? ;
        write_str(w, "\n    ") ? ;
        expr.expr_to_smt2(w, info) ? ;
        write_str(w, "\n  )\n)\n") ?
      }
    }
    Ok(())
  }


  /// Asserts an expression with some print information.
  #[inline]
  pub fn assert_with<Info, Expr>(
    & mut self, expr: & Expr, info: Info
  ) -> SmtRes<()>
  where
  Info: Copy,
  Expr: ?Sized + Expr2Smt<Info> {
    wrt! {
      self, |w| {
        write_str(w, "(assert\n  ") ? ;
        expr.expr_to_smt2(w, info) ? ;
        write_str(w, "\n)\n") ?
      }
    }
    Ok(())
  }

  /// Check-sat assuming command, turns `unknown` results into errors.
  pub fn check_sat_assuming_with<'a, Info, Ident, Idents>(
    & mut self, idents: Idents, info: Info
  ) -> SmtRes<bool>
  where
  Info: Copy,
  Ident: ?Sized + Sym2Smt<Info> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    let future = self.print_check_sat_assuming_with(idents, info) ? ;
    self.parse_check_sat(future)
  }

  /// Check-sat assuming command, turns `unknown` results into `None`.
  pub fn check_sat_assuming_or_unk_with<'a, Info, Ident, Idents>(
    & mut self, idents: Idents, info: Info
  ) -> SmtRes<Option<bool>>
  where
  Info: Copy,
  Ident: ?Sized + Sym2Smt<Info> + 'a,
  Idents: Copy + IntoIterator< Item = & 'a Ident > {
    let future = self.print_check_sat_assuming_with(idents, info) ? ;
    self.parse_check_sat_or_unk(future)
  }

}


/// # Other commands (either untested or not useful right now).
impl<Parser> Solver<Parser> {


  /// Set option command.
  #[inline]
  pub fn set_option<Value: ::std::fmt::Display>(
    & mut self, option: & str, value: Value
  ) -> SmtRes<()> {
    match option {
      ":interactive_mode" => return Err(
        "illegal set-option on interactive mode".into()
      ),
      ":print_success" => return Err(
        "illegal set-option on print success, \
        use `SmtConf` to activate it".into()
      ),
      _ => (),
    } ;
    wrt! {
      self, |w| write!(
        w, "(set-option {} {})\n", option, value
      ) ?
    }
    Ok(())
  }

  /// Activates unsat core production.
  #[inline]
  pub fn produce_unsat_core(& mut self) -> SmtRes<()> {
    self.set_option( ":produce-unsat-cores", "true" )
  }


  /// Resets the assertions in the solver.
  #[inline]
  pub fn reset_assertions(& mut self) -> SmtRes<()> {
    wrt! {
      self, |w| write_str(w, "(reset-assertions)\n") ?
    }
    Ok(())
  }


  // // |===| Inspecting settings.

  /// Get info command.
  #[inline]
  pub fn get_info(& mut self, flag: & str) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(get-info {})\n", flag) ?
    }
    Ok(())
  }
  /// Get option command.
  #[inline]
  pub fn get_option(& mut self, option: & str) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(get-option {})\n", option) ?
    }
    Ok(())
  }



  // |===| Script information.

  /// Set info command.
  #[inline]
  pub fn set_info(& mut self, attribute: & str) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(set-info {})\n", attribute) ?
    }
    Ok(())
  }
  /// Echo command.
  #[inline]
  pub fn echo(& mut self, text: & str) -> SmtRes<()> {
    wrt! {
      self, |w| write!(w, "(echo \"{}\")\n", text) ?
    }
    Ok(())
  }

}










// pub struct DeclareFun<'a, Parser: 'a> {
//   inner: & 'a mut Solver<Parser>,
//   err: Option<Error>,
// }
// impl<'a, Parser: 'a> DeclareFun<'a, Parser> {
//   fn new<FunSym, Info>(
//     inner: & 'a mut Solver<Parser>, sym: & FunSym, info: Info
//   ) -> Self
//   where FunSym: ?Sized + Sym2Smt<Info>, Info: Copy {
//     let err = Self::init(inner, sym, info).err() ;
//     DeclareFun { inner, err }
//   }

//   fn init<FunSym, Info>(
//     inner: & 'a mut Solver<Parser>, sym: & FunSym, info: Info
//   ) -> SmtRes<()>
//   where FunSym: ?Sized + Sym2Smt<Info>, Info: Copy {
//     wrt! {
//       inner, |w| {
//         write!(w, "(declare-fun ") ? ;
//         sym.sym_to_smt2(w, info) ? ;
//         write!(w, " ") ? ;
//       }
//     }
//     Ok(())
//   }

//   pub fn arg<ArgSort: ?Sized + Sort2Smt>(
//     & mut self, arg: & ArgSort
//   ) -> & mut Self {
//     if self.err.is_none() {
//       let err = self.res_arg(arg).err() ;
//       self.err = err
//     }
//     self
//   }

//   pub fn out<OutSort: ?Sized + Sort2Smt>(
//     mut self, out: & OutSort
//   ) -> SmtRes<()> {
//     if self.err.is_none() {
//       let err = self.res_out(out).err() ;
//       self.err = err
//     }

//     if let Some(e) = self.err {
//       Err(e)
//     } else {
//       Ok(())
//     }
//   }

//   pub fn res_arg<ArgSort: ?Sized + Sort2Smt>(
//     & mut self, arg: & ArgSort
//   ) -> SmtRes<()> {
//     wrt! {
//       self.inner, |w| {
//         write!(w, " ") ? ;
//         arg.sort_to_smt2(w) ? ;
//       }
//     }
//     Ok(())
//   }
//   pub fn res_out<OutSort: ?Sized + Sort2Smt>(
//     & mut self, out: & OutSort
//   ) -> SmtRes<()> {
//     wrt! {
//       self.inner, |w| {
//         write!(w, " )") ? ;
//         write!(w, " ") ? ;
//         out.sort_to_smt2(w) ? ;
//         write!(w,")") ?
//       }
//     }
//     Ok(())
//   }
// }