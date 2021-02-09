mod for_loop_arg;
mod for_loop_explicit_counter;
mod for_loop_over_map_kv;
mod for_loop_range;
mod for_mut_range_bound;
mod for_single_element_loop;
mod infinite_loop;
mod manual_flatten;
mod needless_collect;
mod same_item_push;
mod utils;

use crate::utils::sugg::Sugg;
use crate::utils::usage::mutated_variables;
use crate::utils::{
    get_enclosing_block, get_parent_expr, get_trait_def_id, higher, implements_trait, is_in_panic_handler,
    is_integer_const, is_no_std_crate, is_refutable, is_type_diagnostic_item, last_path_segment, match_trait_method,
    path_to_local, path_to_local_id, paths, snippet, snippet_with_applicability, span_lint, span_lint_and_help,
    span_lint_and_sugg, sugg,
};
use if_chain::if_chain;
use rustc_ast::ast;
use rustc_data_structures::fx::FxHashMap;
use rustc_errors::Applicability;
use rustc_hir::intravisit::{walk_block, walk_expr, walk_pat, walk_stmt, NestedVisitorMap, Visitor};
use rustc_hir::{
    BinOpKind, Block, BorrowKind, Expr, ExprKind, HirId, InlineAsmOperand, LoopSource, MatchSource, Mutability, Node,
    Pat, PatKind, Stmt, StmtKind,
};
use rustc_lint::{LateContext, LateLintPass, LintContext};
use rustc_middle::hir::map::Map;
use rustc_middle::lint::in_external_macro;
use rustc_middle::ty::{self, Ty};
use rustc_session::{declare_lint_pass, declare_tool_lint};
use rustc_span::source_map::Span;
use rustc_span::symbol::{sym, Symbol};
use std::iter::{once, Iterator};
use utils::{get_span_of_entire_for_loop, make_iterator_snippet};

declare_clippy_lint! {
    /// **What it does:** Checks for for-loops that manually copy items between
    /// slices that could be optimized by having a memcpy.
    ///
    /// **Why is this bad?** It is not as fast as a memcpy.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```rust
    /// # let src = vec![1];
    /// # let mut dst = vec![0; 65];
    /// for i in 0..src.len() {
    ///     dst[i + 64] = src[i];
    /// }
    /// ```
    /// Could be written as:
    /// ```rust
    /// # let src = vec![1];
    /// # let mut dst = vec![0; 65];
    /// dst[64..(src.len() + 64)].clone_from_slice(&src[..]);
    /// ```
    pub MANUAL_MEMCPY,
    perf,
    "manually copying items between slices"
}

declare_clippy_lint! {
    /// **What it does:** Checks for looping over the range of `0..len` of some
    /// collection just to get the values by index.
    ///
    /// **Why is this bad?** Just iterating the collection itself makes the intent
    /// more clear and is probably faster.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```rust
    /// let vec = vec!['a', 'b', 'c'];
    /// for i in 0..vec.len() {
    ///     println!("{}", vec[i]);
    /// }
    /// ```
    /// Could be written as:
    /// ```rust
    /// let vec = vec!['a', 'b', 'c'];
    /// for i in vec {
    ///     println!("{}", i);
    /// }
    /// ```
    pub NEEDLESS_RANGE_LOOP,
    style,
    "for-looping over a range of indices where an iterator over items would do"
}

declare_clippy_lint! {
    /// **What it does:** Checks for loops on `x.iter()` where `&x` will do, and
    /// suggests the latter.
    ///
    /// **Why is this bad?** Readability.
    ///
    /// **Known problems:** False negatives. We currently only warn on some known
    /// types.
    ///
    /// **Example:**
    /// ```rust
    /// // with `y` a `Vec` or slice:
    /// # let y = vec![1];
    /// for x in y.iter() {
    ///     // ..
    /// }
    /// ```
    /// can be rewritten to
    /// ```rust
    /// # let y = vec![1];
    /// for x in &y {
    ///     // ..
    /// }
    /// ```
    pub EXPLICIT_ITER_LOOP,
    pedantic,
    "for-looping over `_.iter()` or `_.iter_mut()` when `&_` or `&mut _` would do"
}

declare_clippy_lint! {
    /// **What it does:** Checks for loops on `y.into_iter()` where `y` will do, and
    /// suggests the latter.
    ///
    /// **Why is this bad?** Readability.
    ///
    /// **Known problems:** None
    ///
    /// **Example:**
    /// ```rust
    /// # let y = vec![1];
    /// // with `y` a `Vec` or slice:
    /// for x in y.into_iter() {
    ///     // ..
    /// }
    /// ```
    /// can be rewritten to
    /// ```rust
    /// # let y = vec![1];
    /// for x in y {
    ///     // ..
    /// }
    /// ```
    pub EXPLICIT_INTO_ITER_LOOP,
    pedantic,
    "for-looping over `_.into_iter()` when `_` would do"
}

declare_clippy_lint! {
    /// **What it does:** Checks for loops on `x.next()`.
    ///
    /// **Why is this bad?** `next()` returns either `Some(value)` if there was a
    /// value, or `None` otherwise. The insidious thing is that `Option<_>`
    /// implements `IntoIterator`, so that possibly one value will be iterated,
    /// leading to some hard to find bugs. No one will want to write such code
    /// [except to win an Underhanded Rust
    /// Contest](https://www.reddit.com/r/rust/comments/3hb0wm/underhanded_rust_contest/cu5yuhr).
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```ignore
    /// for x in y.next() {
    ///     ..
    /// }
    /// ```
    pub ITER_NEXT_LOOP,
    correctness,
    "for-looping over `_.next()` which is probably not intended"
}

declare_clippy_lint! {
    /// **What it does:** Checks for `for` loops over `Option` or `Result` values.
    ///
    /// **Why is this bad?** Readability. This is more clearly expressed as an `if
    /// let`.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```rust
    /// # let opt = Some(1);
    ///
    /// // Bad
    /// for x in opt {
    ///     // ..
    /// }
    ///
    /// // Good
    /// if let Some(x) = opt {
    ///     // ..
    /// }
    /// ```
    ///
    /// // or
    ///
    /// ```rust
    /// # let res: Result<i32, std::io::Error> = Ok(1);
    ///
    /// // Bad
    /// for x in &res {
    ///     // ..
    /// }
    ///
    /// // Good
    /// if let Ok(x) = res {
    ///     // ..
    /// }
    /// ```
    pub FOR_LOOPS_OVER_FALLIBLES,
    correctness,
    "for-looping over an `Option` or a `Result`, which is more clearly expressed as an `if let`"
}

declare_clippy_lint! {
    /// **What it does:** Detects `loop + match` combinations that are easier
    /// written as a `while let` loop.
    ///
    /// **Why is this bad?** The `while let` loop is usually shorter and more
    /// readable.
    ///
    /// **Known problems:** Sometimes the wrong binding is displayed ([#383](https://github.com/rust-lang/rust-clippy/issues/383)).
    ///
    /// **Example:**
    /// ```rust,no_run
    /// # let y = Some(1);
    /// loop {
    ///     let x = match y {
    ///         Some(x) => x,
    ///         None => break,
    ///     };
    ///     // .. do something with x
    /// }
    /// // is easier written as
    /// while let Some(x) = y {
    ///     // .. do something with x
    /// };
    /// ```
    pub WHILE_LET_LOOP,
    complexity,
    "`loop { if let { ... } else break }`, which can be written as a `while let` loop"
}

declare_clippy_lint! {
    /// **What it does:** Checks for functions collecting an iterator when collect
    /// is not needed.
    ///
    /// **Why is this bad?** `collect` causes the allocation of a new data structure,
    /// when this allocation may not be needed.
    ///
    /// **Known problems:**
    /// None
    ///
    /// **Example:**
    /// ```rust
    /// # let iterator = vec![1].into_iter();
    /// let len = iterator.clone().collect::<Vec<_>>().len();
    /// // should be
    /// let len = iterator.count();
    /// ```
    pub NEEDLESS_COLLECT,
    perf,
    "collecting an iterator when collect is not needed"
}

declare_clippy_lint! {
    /// **What it does:** Checks `for` loops over slices with an explicit counter
    /// and suggests the use of `.enumerate()`.
    ///
    /// **Why is it bad?** Using `.enumerate()` makes the intent more clear,
    /// declutters the code and may be faster in some instances.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```rust
    /// # let v = vec![1];
    /// # fn bar(bar: usize, baz: usize) {}
    /// let mut i = 0;
    /// for item in &v {
    ///     bar(i, *item);
    ///     i += 1;
    /// }
    /// ```
    /// Could be written as
    /// ```rust
    /// # let v = vec![1];
    /// # fn bar(bar: usize, baz: usize) {}
    /// for (i, item) in v.iter().enumerate() { bar(i, *item); }
    /// ```
    pub EXPLICIT_COUNTER_LOOP,
    complexity,
    "for-looping with an explicit counter when `_.enumerate()` would do"
}

declare_clippy_lint! {
    /// **What it does:** Checks for empty `loop` expressions.
    ///
    /// **Why is this bad?** These busy loops burn CPU cycles without doing
    /// anything. It is _almost always_ a better idea to `panic!` than to have
    /// a busy loop.
    ///
    /// If panicking isn't possible, think of the environment and either:
    ///   - block on something
    ///   - sleep the thread for some microseconds
    ///   - yield or pause the thread
    ///
    /// For `std` targets, this can be done with
    /// [`std::thread::sleep`](https://doc.rust-lang.org/std/thread/fn.sleep.html)
    /// or [`std::thread::yield_now`](https://doc.rust-lang.org/std/thread/fn.yield_now.html).
    ///
    /// For `no_std` targets, doing this is more complicated, especially because
    /// `#[panic_handler]`s can't panic. To stop/pause the thread, you will
    /// probably need to invoke some target-specific intrinsic. Examples include:
    ///   - [`x86_64::instructions::hlt`](https://docs.rs/x86_64/0.12.2/x86_64/instructions/fn.hlt.html)
    ///   - [`cortex_m::asm::wfi`](https://docs.rs/cortex-m/0.6.3/cortex_m/asm/fn.wfi.html)
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```no_run
    /// loop {}
    /// ```
    pub EMPTY_LOOP,
    style,
    "empty `loop {}`, which should block or sleep"
}

declare_clippy_lint! {
    /// **What it does:** Checks for `while let` expressions on iterators.
    ///
    /// **Why is this bad?** Readability. A simple `for` loop is shorter and conveys
    /// the intent better.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```ignore
    /// while let Some(val) = iter() {
    ///     ..
    /// }
    /// ```
    pub WHILE_LET_ON_ITERATOR,
    style,
    "using a `while let` loop instead of a for loop on an iterator"
}

declare_clippy_lint! {
    /// **What it does:** Checks for iterating a map (`HashMap` or `BTreeMap`) and
    /// ignoring either the keys or values.
    ///
    /// **Why is this bad?** Readability. There are `keys` and `values` methods that
    /// can be used to express that don't need the values or keys.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    /// ```ignore
    /// for (k, _) in &map {
    ///     ..
    /// }
    /// ```
    ///
    /// could be replaced by
    ///
    /// ```ignore
    /// for k in map.keys() {
    ///     ..
    /// }
    /// ```
    pub FOR_KV_MAP,
    style,
    "looping on a map using `iter` when `keys` or `values` would do"
}

declare_clippy_lint! {
    /// **What it does:** Checks for loops that will always `break`, `return` or
    /// `continue` an outer loop.
    ///
    /// **Why is this bad?** This loop never loops, all it does is obfuscating the
    /// code.
    ///
    /// **Known problems:** None
    ///
    /// **Example:**
    /// ```rust
    /// loop {
    ///     ..;
    ///     break;
    /// }
    /// ```
    pub NEVER_LOOP,
    correctness,
    "any loop that will always `break` or `return`"
}

declare_clippy_lint! {
    /// **What it does:** Checks for loops which have a range bound that is a mutable variable
    ///
    /// **Why is this bad?** One might think that modifying the mutable variable changes the loop bounds
    ///
    /// **Known problems:** None
    ///
    /// **Example:**
    /// ```rust
    /// let mut foo = 42;
    /// for i in 0..foo {
    ///     foo -= 1;
    ///     println!("{}", i); // prints numbers from 0 to 42, not 0 to 21
    /// }
    /// ```
    pub MUT_RANGE_BOUND,
    complexity,
    "for loop over a range where one of the bounds is a mutable variable"
}

declare_clippy_lint! {
    /// **What it does:** Checks whether variables used within while loop condition
    /// can be (and are) mutated in the body.
    ///
    /// **Why is this bad?** If the condition is unchanged, entering the body of the loop
    /// will lead to an infinite loop.
    ///
    /// **Known problems:** If the `while`-loop is in a closure, the check for mutation of the
    /// condition variables in the body can cause false negatives. For example when only `Upvar` `a` is
    /// in the condition and only `Upvar` `b` gets mutated in the body, the lint will not trigger.
    ///
    /// **Example:**
    /// ```rust
    /// let i = 0;
    /// while i > 10 {
    ///     println!("let me loop forever!");
    /// }
    /// ```
    pub WHILE_IMMUTABLE_CONDITION,
    correctness,
    "variables used within while expression are not mutated in the body"
}

declare_clippy_lint! {
    /// **What it does:** Checks whether a for loop is being used to push a constant
    /// value into a Vec.
    ///
    /// **Why is this bad?** This kind of operation can be expressed more succinctly with
    /// `vec![item;SIZE]` or `vec.resize(NEW_SIZE, item)` and using these alternatives may also
    /// have better performance.
    /// **Known problems:** None
    ///
    /// **Example:**
    /// ```rust
    /// let item1 = 2;
    /// let item2 = 3;
    /// let mut vec: Vec<u8> = Vec::new();
    /// for _ in 0..20 {
    ///    vec.push(item1);
    /// }
    /// for _ in 0..30 {
    ///     vec.push(item2);
    /// }
    /// ```
    /// could be written as
    /// ```rust
    /// let item1 = 2;
    /// let item2 = 3;
    /// let mut vec: Vec<u8> = vec![item1; 20];
    /// vec.resize(20 + 30, item2);
    /// ```
    pub SAME_ITEM_PUSH,
    style,
    "the same item is pushed inside of a for loop"
}

declare_clippy_lint! {
    /// **What it does:** Checks whether a for loop has a single element.
    ///
    /// **Why is this bad?** There is no reason to have a loop of a
    /// single element.
    /// **Known problems:** None
    ///
    /// **Example:**
    /// ```rust
    /// let item1 = 2;
    /// for item in &[item1] {
    ///     println!("{}", item);
    /// }
    /// ```
    /// could be written as
    /// ```rust
    /// let item1 = 2;
    /// let item = &item1;
    /// println!("{}", item);
    /// ```
    pub SINGLE_ELEMENT_LOOP,
    complexity,
    "there is no reason to have a single element loop"
}

declare_clippy_lint! {
    /// **What it does:** Check for unnecessary `if let` usage in a for loop
    /// where only the `Some` or `Ok` variant of the iterator element is used.
    ///
    /// **Why is this bad?** It is verbose and can be simplified
    /// by first calling the `flatten` method on the `Iterator`.
    ///
    /// **Known problems:** None.
    ///
    /// **Example:**
    ///
    /// ```rust
    /// let x = vec![Some(1), Some(2), Some(3)];
    /// for n in x {
    ///     if let Some(n) = n {
    ///         println!("{}", n);
    ///     }
    /// }
    /// ```
    /// Use instead:
    /// ```rust
    /// let x = vec![Some(1), Some(2), Some(3)];
    /// for n in x.into_iter().flatten() {
    ///     println!("{}", n);
    /// }
    /// ```
    pub MANUAL_FLATTEN,
    complexity,
    "for loops over `Option`s or `Result`s with a single expression can be simplified"
}

declare_lint_pass!(Loops => [
    MANUAL_MEMCPY,
    MANUAL_FLATTEN,
    NEEDLESS_RANGE_LOOP,
    EXPLICIT_ITER_LOOP,
    EXPLICIT_INTO_ITER_LOOP,
    ITER_NEXT_LOOP,
    FOR_LOOPS_OVER_FALLIBLES,
    WHILE_LET_LOOP,
    NEEDLESS_COLLECT,
    EXPLICIT_COUNTER_LOOP,
    EMPTY_LOOP,
    WHILE_LET_ON_ITERATOR,
    FOR_KV_MAP,
    NEVER_LOOP,
    MUT_RANGE_BOUND,
    WHILE_IMMUTABLE_CONDITION,
    SAME_ITEM_PUSH,
    SINGLE_ELEMENT_LOOP,
]);

impl<'tcx> LateLintPass<'tcx> for Loops {
    #[allow(clippy::too_many_lines)]
    fn check_expr(&mut self, cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>) {
        if let Some((pat, arg, body, span)) = higher::for_loop(expr) {
            // we don't want to check expanded macros
            // this check is not at the top of the function
            // since higher::for_loop expressions are marked as expansions
            if body.span.from_expansion() {
                return;
            }
            check_for_loop(cx, pat, arg, body, expr, span);
        }

        // we don't want to check expanded macros
        if expr.span.from_expansion() {
            return;
        }

        // check for never_loop
        if let ExprKind::Loop(ref block, _, _, _) = expr.kind {
            match never_loop_block(block, expr.hir_id) {
                NeverLoopResult::AlwaysBreak => span_lint(cx, NEVER_LOOP, expr.span, "this loop never actually loops"),
                NeverLoopResult::MayContinueMainLoop | NeverLoopResult::Otherwise => (),
            }
        }

        // check for `loop { if let {} else break }` that could be `while let`
        // (also matches an explicit "match" instead of "if let")
        // (even if the "match" or "if let" is used for declaration)
        if let ExprKind::Loop(ref block, _, LoopSource::Loop, _) = expr.kind {
            // also check for empty `loop {}` statements, skipping those in #[panic_handler]
            if block.stmts.is_empty() && block.expr.is_none() && !is_in_panic_handler(cx, expr) {
                let msg = "empty `loop {}` wastes CPU cycles";
                let help = if is_no_std_crate(cx.tcx.hir().krate()) {
                    "you should either use `panic!()` or add a call pausing or sleeping the thread to the loop body"
                } else {
                    "you should either use `panic!()` or add `std::thread::sleep(..);` to the loop body"
                };
                span_lint_and_help(cx, EMPTY_LOOP, expr.span, msg, None, help);
            }

            // extract the expression from the first statement (if any) in a block
            let inner_stmt_expr = extract_expr_from_first_stmt(block);
            // or extract the first expression (if any) from the block
            if let Some(inner) = inner_stmt_expr.or_else(|| extract_first_expr(block)) {
                if let ExprKind::Match(ref matchexpr, ref arms, ref source) = inner.kind {
                    // ensure "if let" compatible match structure
                    match *source {
                        MatchSource::Normal | MatchSource::IfLetDesugar { .. } => {
                            if arms.len() == 2
                                && arms[0].guard.is_none()
                                && arms[1].guard.is_none()
                                && is_simple_break_expr(&arms[1].body)
                            {
                                if in_external_macro(cx.sess(), expr.span) {
                                    return;
                                }

                                // NOTE: we used to build a body here instead of using
                                // ellipsis, this was removed because:
                                // 1) it was ugly with big bodies;
                                // 2) it was not indented properly;
                                // 3) it wasn’t very smart (see #675).
                                let mut applicability = Applicability::HasPlaceholders;
                                span_lint_and_sugg(
                                    cx,
                                    WHILE_LET_LOOP,
                                    expr.span,
                                    "this loop could be written as a `while let` loop",
                                    "try",
                                    format!(
                                        "while let {} = {} {{ .. }}",
                                        snippet_with_applicability(cx, arms[0].pat.span, "..", &mut applicability),
                                        snippet_with_applicability(cx, matchexpr.span, "..", &mut applicability),
                                    ),
                                    applicability,
                                );
                            }
                        },
                        _ => (),
                    }
                }
            }
        }
        if let ExprKind::Match(ref match_expr, ref arms, MatchSource::WhileLetDesugar) = expr.kind {
            let pat = &arms[0].pat.kind;
            if let (
                &PatKind::TupleStruct(ref qpath, ref pat_args, _),
                &ExprKind::MethodCall(ref method_path, _, ref method_args, _),
            ) = (pat, &match_expr.kind)
            {
                let iter_expr = &method_args[0];

                // Don't lint when the iterator is recreated on every iteration
                if_chain! {
                    if let ExprKind::MethodCall(..) | ExprKind::Call(..) = iter_expr.kind;
                    if let Some(iter_def_id) = get_trait_def_id(cx, &paths::ITERATOR);
                    if implements_trait(cx, cx.typeck_results().expr_ty(iter_expr), iter_def_id, &[]);
                    then {
                        return;
                    }
                }

                let lhs_constructor = last_path_segment(qpath);
                if method_path.ident.name == sym::next
                    && match_trait_method(cx, match_expr, &paths::ITERATOR)
                    && lhs_constructor.ident.name == sym::Some
                    && (pat_args.is_empty()
                        || !is_refutable(cx, &pat_args[0])
                            && !is_used_inside(cx, iter_expr, &arms[0].body)
                            && !is_iterator_used_after_while_let(cx, iter_expr)
                            && !is_nested(cx, expr, &method_args[0]))
                {
                    let mut applicability = Applicability::MachineApplicable;
                    let iterator = snippet_with_applicability(cx, method_args[0].span, "_", &mut applicability);
                    let loop_var = if pat_args.is_empty() {
                        "_".to_string()
                    } else {
                        snippet_with_applicability(cx, pat_args[0].span, "_", &mut applicability).into_owned()
                    };
                    span_lint_and_sugg(
                        cx,
                        WHILE_LET_ON_ITERATOR,
                        expr.span.with_hi(match_expr.span.hi()),
                        "this loop could be written as a `for` loop",
                        "try",
                        format!("for {} in {}", loop_var, iterator),
                        applicability,
                    );
                }
            }
        }

        if let Some((cond, body)) = higher::while_loop(&expr) {
            infinite_loop::check_infinite_loop(cx, cond, body);
        }

        needless_collect::check_needless_collect(expr, cx);
    }
}

enum NeverLoopResult {
    // A break/return always get triggered but not necessarily for the main loop.
    AlwaysBreak,
    // A continue may occur for the main loop.
    MayContinueMainLoop,
    Otherwise,
}

#[must_use]
fn absorb_break(arg: &NeverLoopResult) -> NeverLoopResult {
    match *arg {
        NeverLoopResult::AlwaysBreak | NeverLoopResult::Otherwise => NeverLoopResult::Otherwise,
        NeverLoopResult::MayContinueMainLoop => NeverLoopResult::MayContinueMainLoop,
    }
}

// Combine two results for parts that are called in order.
#[must_use]
fn combine_seq(first: NeverLoopResult, second: NeverLoopResult) -> NeverLoopResult {
    match first {
        NeverLoopResult::AlwaysBreak | NeverLoopResult::MayContinueMainLoop => first,
        NeverLoopResult::Otherwise => second,
    }
}

// Combine two results where both parts are called but not necessarily in order.
#[must_use]
fn combine_both(left: NeverLoopResult, right: NeverLoopResult) -> NeverLoopResult {
    match (left, right) {
        (NeverLoopResult::MayContinueMainLoop, _) | (_, NeverLoopResult::MayContinueMainLoop) => {
            NeverLoopResult::MayContinueMainLoop
        },
        (NeverLoopResult::AlwaysBreak, _) | (_, NeverLoopResult::AlwaysBreak) => NeverLoopResult::AlwaysBreak,
        (NeverLoopResult::Otherwise, NeverLoopResult::Otherwise) => NeverLoopResult::Otherwise,
    }
}

// Combine two results where only one of the part may have been executed.
#[must_use]
fn combine_branches(b1: NeverLoopResult, b2: NeverLoopResult) -> NeverLoopResult {
    match (b1, b2) {
        (NeverLoopResult::AlwaysBreak, NeverLoopResult::AlwaysBreak) => NeverLoopResult::AlwaysBreak,
        (NeverLoopResult::MayContinueMainLoop, _) | (_, NeverLoopResult::MayContinueMainLoop) => {
            NeverLoopResult::MayContinueMainLoop
        },
        (NeverLoopResult::Otherwise, _) | (_, NeverLoopResult::Otherwise) => NeverLoopResult::Otherwise,
    }
}

fn never_loop_block(block: &Block<'_>, main_loop_id: HirId) -> NeverLoopResult {
    let stmts = block.stmts.iter().map(stmt_to_expr);
    let expr = once(block.expr.as_deref());
    let mut iter = stmts.chain(expr).flatten();
    never_loop_expr_seq(&mut iter, main_loop_id)
}

fn stmt_to_expr<'tcx>(stmt: &Stmt<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    match stmt.kind {
        StmtKind::Semi(ref e, ..) | StmtKind::Expr(ref e, ..) => Some(e),
        StmtKind::Local(ref local) => local.init.as_deref(),
        _ => None,
    }
}

fn never_loop_expr(expr: &Expr<'_>, main_loop_id: HirId) -> NeverLoopResult {
    match expr.kind {
        ExprKind::Box(ref e)
        | ExprKind::Unary(_, ref e)
        | ExprKind::Cast(ref e, _)
        | ExprKind::Type(ref e, _)
        | ExprKind::Field(ref e, _)
        | ExprKind::AddrOf(_, _, ref e)
        | ExprKind::Struct(_, _, Some(ref e))
        | ExprKind::Repeat(ref e, _)
        | ExprKind::DropTemps(ref e) => never_loop_expr(e, main_loop_id),
        ExprKind::Array(ref es) | ExprKind::MethodCall(_, _, ref es, _) | ExprKind::Tup(ref es) => {
            never_loop_expr_all(&mut es.iter(), main_loop_id)
        },
        ExprKind::Call(ref e, ref es) => never_loop_expr_all(&mut once(&**e).chain(es.iter()), main_loop_id),
        ExprKind::Binary(_, ref e1, ref e2)
        | ExprKind::Assign(ref e1, ref e2, _)
        | ExprKind::AssignOp(_, ref e1, ref e2)
        | ExprKind::Index(ref e1, ref e2) => never_loop_expr_all(&mut [&**e1, &**e2].iter().cloned(), main_loop_id),
        ExprKind::Loop(ref b, _, _, _) => {
            // Break can come from the inner loop so remove them.
            absorb_break(&never_loop_block(b, main_loop_id))
        },
        ExprKind::If(ref e, ref e2, ref e3) => {
            let e1 = never_loop_expr(e, main_loop_id);
            let e2 = never_loop_expr(e2, main_loop_id);
            let e3 = e3
                .as_ref()
                .map_or(NeverLoopResult::Otherwise, |e| never_loop_expr(e, main_loop_id));
            combine_seq(e1, combine_branches(e2, e3))
        },
        ExprKind::Match(ref e, ref arms, _) => {
            let e = never_loop_expr(e, main_loop_id);
            if arms.is_empty() {
                e
            } else {
                let arms = never_loop_expr_branch(&mut arms.iter().map(|a| &*a.body), main_loop_id);
                combine_seq(e, arms)
            }
        },
        ExprKind::Block(ref b, _) => never_loop_block(b, main_loop_id),
        ExprKind::Continue(d) => {
            let id = d
                .target_id
                .expect("target ID can only be missing in the presence of compilation errors");
            if id == main_loop_id {
                NeverLoopResult::MayContinueMainLoop
            } else {
                NeverLoopResult::AlwaysBreak
            }
        },
        ExprKind::Break(_, ref e) | ExprKind::Ret(ref e) => e.as_ref().map_or(NeverLoopResult::AlwaysBreak, |e| {
            combine_seq(never_loop_expr(e, main_loop_id), NeverLoopResult::AlwaysBreak)
        }),
        ExprKind::InlineAsm(ref asm) => asm
            .operands
            .iter()
            .map(|(o, _)| match o {
                InlineAsmOperand::In { expr, .. }
                | InlineAsmOperand::InOut { expr, .. }
                | InlineAsmOperand::Const { expr }
                | InlineAsmOperand::Sym { expr } => never_loop_expr(expr, main_loop_id),
                InlineAsmOperand::Out { expr, .. } => never_loop_expr_all(&mut expr.iter(), main_loop_id),
                InlineAsmOperand::SplitInOut { in_expr, out_expr, .. } => {
                    never_loop_expr_all(&mut once(in_expr).chain(out_expr.iter()), main_loop_id)
                },
            })
            .fold(NeverLoopResult::Otherwise, combine_both),
        ExprKind::Struct(_, _, None)
        | ExprKind::Yield(_, _)
        | ExprKind::Closure(_, _, _, _, _)
        | ExprKind::LlvmInlineAsm(_)
        | ExprKind::Path(_)
        | ExprKind::ConstBlock(_)
        | ExprKind::Lit(_)
        | ExprKind::Err => NeverLoopResult::Otherwise,
    }
}

fn never_loop_expr_seq<'a, T: Iterator<Item = &'a Expr<'a>>>(es: &mut T, main_loop_id: HirId) -> NeverLoopResult {
    es.map(|e| never_loop_expr(e, main_loop_id))
        .fold(NeverLoopResult::Otherwise, combine_seq)
}

fn never_loop_expr_all<'a, T: Iterator<Item = &'a Expr<'a>>>(es: &mut T, main_loop_id: HirId) -> NeverLoopResult {
    es.map(|e| never_loop_expr(e, main_loop_id))
        .fold(NeverLoopResult::Otherwise, combine_both)
}

fn never_loop_expr_branch<'a, T: Iterator<Item = &'a Expr<'a>>>(e: &mut T, main_loop_id: HirId) -> NeverLoopResult {
    e.map(|e| never_loop_expr(e, main_loop_id))
        .fold(NeverLoopResult::AlwaysBreak, combine_branches)
}

fn check_for_loop<'tcx>(
    cx: &LateContext<'tcx>,
    pat: &'tcx Pat<'_>,
    arg: &'tcx Expr<'_>,
    body: &'tcx Expr<'_>,
    expr: &'tcx Expr<'_>,
    span: Span,
) {
    let is_manual_memcpy_triggered = detect_manual_memcpy(cx, pat, arg, body, expr);
    if !is_manual_memcpy_triggered {
        for_loop_range::check_for_loop_range(cx, pat, arg, body, expr);
        for_loop_explicit_counter::check_for_loop_explicit_counter(cx, pat, arg, body, expr);
    }
    for_loop_arg::check_for_loop_arg(cx, pat, arg, expr);
    for_loop_over_map_kv::check_for_loop_over_map_kv(cx, pat, arg, body, expr);
    for_mut_range_bound::check_for_mut_range_bound(cx, arg, body);
    for_single_element_loop::check_for_single_element_loop(cx, pat, arg, body, expr);
    same_item_push::detect_same_item_push(cx, pat, arg, body, expr);
    manual_flatten::check_manual_flatten(cx, pat, arg, body, span);
}

/// a wrapper of `Sugg`. Besides what `Sugg` do, this removes unnecessary `0`;
/// and also, it avoids subtracting a variable from the same one by replacing it with `0`.
/// it exists for the convenience of the overloaded operators while normal functions can do the
/// same.
#[derive(Clone)]
struct MinifyingSugg<'a>(Sugg<'a>);

impl<'a> MinifyingSugg<'a> {
    fn as_str(&self) -> &str {
        let Sugg::NonParen(s) | Sugg::MaybeParen(s) | Sugg::BinOp(_, s) = &self.0;
        s.as_ref()
    }

    fn into_sugg(self) -> Sugg<'a> {
        self.0
    }
}

impl<'a> From<Sugg<'a>> for MinifyingSugg<'a> {
    fn from(sugg: Sugg<'a>) -> Self {
        Self(sugg)
    }
}

impl std::ops::Add for &MinifyingSugg<'static> {
    type Output = MinifyingSugg<'static>;
    fn add(self, rhs: &MinifyingSugg<'static>) -> MinifyingSugg<'static> {
        match (self.as_str(), rhs.as_str()) {
            ("0", _) => rhs.clone(),
            (_, "0") => self.clone(),
            (_, _) => (&self.0 + &rhs.0).into(),
        }
    }
}

impl std::ops::Sub for &MinifyingSugg<'static> {
    type Output = MinifyingSugg<'static>;
    fn sub(self, rhs: &MinifyingSugg<'static>) -> MinifyingSugg<'static> {
        match (self.as_str(), rhs.as_str()) {
            (_, "0") => self.clone(),
            ("0", _) => (-rhs.0.clone()).into(),
            (x, y) if x == y => sugg::ZERO.into(),
            (_, _) => (&self.0 - &rhs.0).into(),
        }
    }
}

impl std::ops::Add<&MinifyingSugg<'static>> for MinifyingSugg<'static> {
    type Output = MinifyingSugg<'static>;
    fn add(self, rhs: &MinifyingSugg<'static>) -> MinifyingSugg<'static> {
        match (self.as_str(), rhs.as_str()) {
            ("0", _) => rhs.clone(),
            (_, "0") => self,
            (_, _) => (self.0 + &rhs.0).into(),
        }
    }
}

impl std::ops::Sub<&MinifyingSugg<'static>> for MinifyingSugg<'static> {
    type Output = MinifyingSugg<'static>;
    fn sub(self, rhs: &MinifyingSugg<'static>) -> MinifyingSugg<'static> {
        match (self.as_str(), rhs.as_str()) {
            (_, "0") => self,
            ("0", _) => (-rhs.0.clone()).into(),
            (x, y) if x == y => sugg::ZERO.into(),
            (_, _) => (self.0 - &rhs.0).into(),
        }
    }
}

/// a wrapper around `MinifyingSugg`, which carries a operator like currying
/// so that the suggested code become more efficient (e.g. `foo + -bar` `foo - bar`).
struct Offset {
    value: MinifyingSugg<'static>,
    sign: OffsetSign,
}

#[derive(Clone, Copy)]
enum OffsetSign {
    Positive,
    Negative,
}

impl Offset {
    fn negative(value: Sugg<'static>) -> Self {
        Self {
            value: value.into(),
            sign: OffsetSign::Negative,
        }
    }

    fn positive(value: Sugg<'static>) -> Self {
        Self {
            value: value.into(),
            sign: OffsetSign::Positive,
        }
    }

    fn empty() -> Self {
        Self::positive(sugg::ZERO)
    }
}

fn apply_offset(lhs: &MinifyingSugg<'static>, rhs: &Offset) -> MinifyingSugg<'static> {
    match rhs.sign {
        OffsetSign::Positive => lhs + &rhs.value,
        OffsetSign::Negative => lhs - &rhs.value,
    }
}

#[derive(Debug, Clone, Copy)]
enum StartKind<'hir> {
    Range,
    Counter { initializer: &'hir Expr<'hir> },
}

struct IndexExpr<'hir> {
    base: &'hir Expr<'hir>,
    idx: StartKind<'hir>,
    idx_offset: Offset,
}

struct Start<'hir> {
    id: HirId,
    kind: StartKind<'hir>,
}

fn is_slice_like<'tcx>(cx: &LateContext<'tcx>, ty: Ty<'_>) -> bool {
    let is_slice = match ty.kind() {
        ty::Ref(_, subty, _) => is_slice_like(cx, subty),
        ty::Slice(..) | ty::Array(..) => true,
        _ => false,
    };

    is_slice || is_type_diagnostic_item(cx, ty, sym::vec_type) || is_type_diagnostic_item(cx, ty, sym!(vecdeque_type))
}

fn fetch_cloned_expr<'tcx>(expr: &'tcx Expr<'tcx>) -> &'tcx Expr<'tcx> {
    if_chain! {
        if let ExprKind::MethodCall(method, _, args, _) = expr.kind;
        if method.ident.name == sym::clone;
        if args.len() == 1;
        if let Some(arg) = args.get(0);
        then { arg } else { expr }
    }
}

fn get_details_from_idx<'tcx>(
    cx: &LateContext<'tcx>,
    idx: &Expr<'_>,
    starts: &[Start<'tcx>],
) -> Option<(StartKind<'tcx>, Offset)> {
    fn get_start<'tcx>(e: &Expr<'_>, starts: &[Start<'tcx>]) -> Option<StartKind<'tcx>> {
        let id = path_to_local(e)?;
        starts.iter().find(|start| start.id == id).map(|start| start.kind)
    }

    fn get_offset<'tcx>(cx: &LateContext<'tcx>, e: &Expr<'_>, starts: &[Start<'tcx>]) -> Option<Sugg<'static>> {
        match &e.kind {
            ExprKind::Lit(l) => match l.node {
                ast::LitKind::Int(x, _ty) => Some(Sugg::NonParen(x.to_string().into())),
                _ => None,
            },
            ExprKind::Path(..) if get_start(e, starts).is_none() => Some(Sugg::hir(cx, e, "???")),
            _ => None,
        }
    }

    match idx.kind {
        ExprKind::Binary(op, lhs, rhs) => match op.node {
            BinOpKind::Add => {
                let offset_opt = get_start(lhs, starts)
                    .and_then(|s| get_offset(cx, rhs, starts).map(|o| (s, o)))
                    .or_else(|| get_start(rhs, starts).and_then(|s| get_offset(cx, lhs, starts).map(|o| (s, o))));

                offset_opt.map(|(s, o)| (s, Offset::positive(o)))
            },
            BinOpKind::Sub => {
                get_start(lhs, starts).and_then(|s| get_offset(cx, rhs, starts).map(|o| (s, Offset::negative(o))))
            },
            _ => None,
        },
        ExprKind::Path(..) => get_start(idx, starts).map(|s| (s, Offset::empty())),
        _ => None,
    }
}

fn get_assignment<'tcx>(e: &'tcx Expr<'tcx>) -> Option<(&'tcx Expr<'tcx>, &'tcx Expr<'tcx>)> {
    if let ExprKind::Assign(lhs, rhs, _) = e.kind {
        Some((lhs, rhs))
    } else {
        None
    }
}

/// Get assignments from the given block.
/// The returned iterator yields `None` if no assignment expressions are there,
/// filtering out the increments of the given whitelisted loop counters;
/// because its job is to make sure there's nothing other than assignments and the increments.
fn get_assignments<'a, 'tcx>(
    Block { stmts, expr, .. }: &'tcx Block<'tcx>,
    loop_counters: &'a [Start<'tcx>],
) -> impl Iterator<Item = Option<(&'tcx Expr<'tcx>, &'tcx Expr<'tcx>)>> + 'a {
    // As the `filter` and `map` below do different things, I think putting together
    // just increases complexity. (cc #3188 and #4193)
    stmts
        .iter()
        .filter_map(move |stmt| match stmt.kind {
            StmtKind::Local(..) | StmtKind::Item(..) => None,
            StmtKind::Expr(e) | StmtKind::Semi(e) => Some(e),
        })
        .chain((*expr).into_iter())
        .filter(move |e| {
            if let ExprKind::AssignOp(_, place, _) = e.kind {
                path_to_local(place).map_or(false, |id| {
                    !loop_counters
                        .iter()
                        // skip the first item which should be `StartKind::Range`
                        // this makes it possible to use the slice with `StartKind::Range` in the same iterator loop.
                        .skip(1)
                        .any(|counter| counter.id == id)
                })
            } else {
                true
            }
        })
        .map(get_assignment)
}

fn get_loop_counters<'a, 'tcx>(
    cx: &'a LateContext<'tcx>,
    body: &'tcx Block<'tcx>,
    expr: &'tcx Expr<'_>,
) -> Option<impl Iterator<Item = Start<'tcx>> + 'a> {
    // Look for variables that are incremented once per loop iteration.
    let mut increment_visitor = IncrementVisitor::new(cx);
    walk_block(&mut increment_visitor, body);

    // For each candidate, check the parent block to see if
    // it's initialized to zero at the start of the loop.
    get_enclosing_block(&cx, expr.hir_id).and_then(|block| {
        increment_visitor
            .into_results()
            .filter_map(move |var_id| {
                let mut initialize_visitor = InitializeVisitor::new(cx, expr, var_id);
                walk_block(&mut initialize_visitor, block);

                initialize_visitor.get_result().map(|(_, initializer)| Start {
                    id: var_id,
                    kind: StartKind::Counter { initializer },
                })
            })
            .into()
    })
}

fn build_manual_memcpy_suggestion<'tcx>(
    cx: &LateContext<'tcx>,
    start: &Expr<'_>,
    end: &Expr<'_>,
    limits: ast::RangeLimits,
    dst: &IndexExpr<'_>,
    src: &IndexExpr<'_>,
) -> String {
    fn print_offset(offset: MinifyingSugg<'static>) -> MinifyingSugg<'static> {
        if offset.as_str() == "0" {
            sugg::EMPTY.into()
        } else {
            offset
        }
    }

    let print_limit = |end: &Expr<'_>, end_str: &str, base: &Expr<'_>, sugg: MinifyingSugg<'static>| {
        if_chain! {
            if let ExprKind::MethodCall(method, _, len_args, _) = end.kind;
            if method.ident.name == sym!(len);
            if len_args.len() == 1;
            if let Some(arg) = len_args.get(0);
            if path_to_local(arg) == path_to_local(base);
            then {
                if sugg.as_str() == end_str {
                    sugg::EMPTY.into()
                } else {
                    sugg
                }
            } else {
                match limits {
                    ast::RangeLimits::Closed => {
                        sugg + &sugg::ONE.into()
                    },
                    ast::RangeLimits::HalfOpen => sugg,
                }
            }
        }
    };

    let start_str = Sugg::hir(cx, start, "").into();
    let end_str: MinifyingSugg<'_> = Sugg::hir(cx, end, "").into();

    let print_offset_and_limit = |idx_expr: &IndexExpr<'_>| match idx_expr.idx {
        StartKind::Range => (
            print_offset(apply_offset(&start_str, &idx_expr.idx_offset)).into_sugg(),
            print_limit(
                end,
                end_str.as_str(),
                idx_expr.base,
                apply_offset(&end_str, &idx_expr.idx_offset),
            )
            .into_sugg(),
        ),
        StartKind::Counter { initializer } => {
            let counter_start = Sugg::hir(cx, initializer, "").into();
            (
                print_offset(apply_offset(&counter_start, &idx_expr.idx_offset)).into_sugg(),
                print_limit(
                    end,
                    end_str.as_str(),
                    idx_expr.base,
                    apply_offset(&end_str, &idx_expr.idx_offset) + &counter_start - &start_str,
                )
                .into_sugg(),
            )
        },
    };

    let (dst_offset, dst_limit) = print_offset_and_limit(&dst);
    let (src_offset, src_limit) = print_offset_and_limit(&src);

    let dst_base_str = snippet(cx, dst.base.span, "???");
    let src_base_str = snippet(cx, src.base.span, "???");

    let dst = if dst_offset == sugg::EMPTY && dst_limit == sugg::EMPTY {
        dst_base_str
    } else {
        format!(
            "{}[{}..{}]",
            dst_base_str,
            dst_offset.maybe_par(),
            dst_limit.maybe_par()
        )
        .into()
    };

    format!(
        "{}.clone_from_slice(&{}[{}..{}]);",
        dst,
        src_base_str,
        src_offset.maybe_par(),
        src_limit.maybe_par()
    )
}

/// Checks for for loops that sequentially copy items from one slice-like
/// object to another.
fn detect_manual_memcpy<'tcx>(
    cx: &LateContext<'tcx>,
    pat: &'tcx Pat<'_>,
    arg: &'tcx Expr<'_>,
    body: &'tcx Expr<'_>,
    expr: &'tcx Expr<'_>,
) -> bool {
    if let Some(higher::Range {
        start: Some(start),
        end: Some(end),
        limits,
    }) = higher::range(arg)
    {
        // the var must be a single name
        if let PatKind::Binding(_, canonical_id, _, _) = pat.kind {
            let mut starts = vec![Start {
                id: canonical_id,
                kind: StartKind::Range,
            }];

            // This is one of few ways to return different iterators
            // derived from: https://stackoverflow.com/questions/29760668/conditionally-iterate-over-one-of-several-possible-iterators/52064434#52064434
            let mut iter_a = None;
            let mut iter_b = None;

            if let ExprKind::Block(block, _) = body.kind {
                if let Some(loop_counters) = get_loop_counters(cx, block, expr) {
                    starts.extend(loop_counters);
                }
                iter_a = Some(get_assignments(block, &starts));
            } else {
                iter_b = Some(get_assignment(body));
            }

            let assignments = iter_a.into_iter().flatten().chain(iter_b.into_iter());

            let big_sugg = assignments
                // The only statements in the for loops can be indexed assignments from
                // indexed retrievals (except increments of loop counters).
                .map(|o| {
                    o.and_then(|(lhs, rhs)| {
                        let rhs = fetch_cloned_expr(rhs);
                        if_chain! {
                            if let ExprKind::Index(base_left, idx_left) = lhs.kind;
                            if let ExprKind::Index(base_right, idx_right) = rhs.kind;
                            if is_slice_like(cx, cx.typeck_results().expr_ty(base_left))
                                && is_slice_like(cx, cx.typeck_results().expr_ty(base_right));
                            if let Some((start_left, offset_left)) = get_details_from_idx(cx, &idx_left, &starts);
                            if let Some((start_right, offset_right)) = get_details_from_idx(cx, &idx_right, &starts);

                            // Source and destination must be different
                            if path_to_local(base_left) != path_to_local(base_right);
                            then {
                                Some((IndexExpr { base: base_left, idx: start_left, idx_offset: offset_left },
                                    IndexExpr { base: base_right, idx: start_right, idx_offset: offset_right }))
                            } else {
                                None
                            }
                        }
                    })
                })
                .map(|o| o.map(|(dst, src)| build_manual_memcpy_suggestion(cx, start, end, limits, &dst, &src)))
                .collect::<Option<Vec<_>>>()
                .filter(|v| !v.is_empty())
                .map(|v| v.join("\n    "));

            if let Some(big_sugg) = big_sugg {
                span_lint_and_sugg(
                    cx,
                    MANUAL_MEMCPY,
                    get_span_of_entire_for_loop(expr),
                    "it looks like you're manually copying between slices",
                    "try replacing the loop by",
                    big_sugg,
                    Applicability::Unspecified,
                );
                return true;
            }
        }
    }
    false
}

fn is_used_inside<'tcx>(cx: &LateContext<'tcx>, expr: &'tcx Expr<'_>, container: &'tcx Expr<'_>) -> bool {
    let def_id = match path_to_local(expr) {
        Some(id) => id,
        None => return false,
    };
    if let Some(used_mutably) = mutated_variables(container, cx) {
        if used_mutably.contains(&def_id) {
            return true;
        }
    }
    false
}

fn is_iterator_used_after_while_let<'tcx>(cx: &LateContext<'tcx>, iter_expr: &'tcx Expr<'_>) -> bool {
    let def_id = match path_to_local(iter_expr) {
        Some(id) => id,
        None => return false,
    };
    let mut visitor = VarUsedAfterLoopVisitor {
        def_id,
        iter_expr_id: iter_expr.hir_id,
        past_while_let: false,
        var_used_after_while_let: false,
    };
    if let Some(enclosing_block) = get_enclosing_block(cx, def_id) {
        walk_block(&mut visitor, enclosing_block);
    }
    visitor.var_used_after_while_let
}

struct VarUsedAfterLoopVisitor {
    def_id: HirId,
    iter_expr_id: HirId,
    past_while_let: bool,
    var_used_after_while_let: bool,
}

impl<'tcx> Visitor<'tcx> for VarUsedAfterLoopVisitor {
    type Map = Map<'tcx>;

    fn visit_expr(&mut self, expr: &'tcx Expr<'_>) {
        if self.past_while_let {
            if path_to_local_id(expr, self.def_id) {
                self.var_used_after_while_let = true;
            }
        } else if self.iter_expr_id == expr.hir_id {
            self.past_while_let = true;
        }
        walk_expr(self, expr);
    }
    fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
        NestedVisitorMap::None
    }
}

/// If a block begins with a statement (possibly a `let` binding) and has an
/// expression, return it.
fn extract_expr_from_first_stmt<'tcx>(block: &Block<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    if block.stmts.is_empty() {
        return None;
    }
    if let StmtKind::Local(ref local) = block.stmts[0].kind {
        local.init //.map(|expr| expr)
    } else {
        None
    }
}

/// If a block begins with an expression (with or without semicolon), return it.
fn extract_first_expr<'tcx>(block: &Block<'tcx>) -> Option<&'tcx Expr<'tcx>> {
    match block.expr {
        Some(ref expr) if block.stmts.is_empty() => Some(expr),
        None if !block.stmts.is_empty() => match block.stmts[0].kind {
            StmtKind::Expr(ref expr) | StmtKind::Semi(ref expr) => Some(expr),
            StmtKind::Local(..) | StmtKind::Item(..) => None,
        },
        _ => None,
    }
}

/// Returns `true` if expr contains a single break expr without destination label
/// and
/// passed expression. The expression may be within a block.
fn is_simple_break_expr(expr: &Expr<'_>) -> bool {
    match expr.kind {
        ExprKind::Break(dest, ref passed_expr) if dest.label.is_none() && passed_expr.is_none() => true,
        ExprKind::Block(ref b, _) => extract_first_expr(b).map_or(false, |subexpr| is_simple_break_expr(subexpr)),
        _ => false,
    }
}

#[derive(Debug, PartialEq)]
enum IncrementVisitorVarState {
    Initial,  // Not examined yet
    IncrOnce, // Incremented exactly once, may be a loop counter
    DontWarn,
}

/// Scan a for loop for variables that are incremented exactly once and not used after that.
struct IncrementVisitor<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,                          // context reference
    states: FxHashMap<HirId, IncrementVisitorVarState>, // incremented variables
    depth: u32,                                         // depth of conditional expressions
    done: bool,
}

impl<'a, 'tcx> IncrementVisitor<'a, 'tcx> {
    fn new(cx: &'a LateContext<'tcx>) -> Self {
        Self {
            cx,
            states: FxHashMap::default(),
            depth: 0,
            done: false,
        }
    }

    fn into_results(self) -> impl Iterator<Item = HirId> {
        self.states.into_iter().filter_map(|(id, state)| {
            if state == IncrementVisitorVarState::IncrOnce {
                Some(id)
            } else {
                None
            }
        })
    }
}

impl<'a, 'tcx> Visitor<'tcx> for IncrementVisitor<'a, 'tcx> {
    type Map = Map<'tcx>;

    fn visit_expr(&mut self, expr: &'tcx Expr<'_>) {
        if self.done {
            return;
        }

        // If node is a variable
        if let Some(def_id) = path_to_local(expr) {
            if let Some(parent) = get_parent_expr(self.cx, expr) {
                let state = self.states.entry(def_id).or_insert(IncrementVisitorVarState::Initial);
                if *state == IncrementVisitorVarState::IncrOnce {
                    *state = IncrementVisitorVarState::DontWarn;
                    return;
                }

                match parent.kind {
                    ExprKind::AssignOp(op, ref lhs, ref rhs) => {
                        if lhs.hir_id == expr.hir_id {
                            *state = if op.node == BinOpKind::Add
                                && is_integer_const(self.cx, rhs, 1)
                                && *state == IncrementVisitorVarState::Initial
                                && self.depth == 0
                            {
                                IncrementVisitorVarState::IncrOnce
                            } else {
                                // Assigned some other value or assigned multiple times
                                IncrementVisitorVarState::DontWarn
                            };
                        }
                    },
                    ExprKind::Assign(ref lhs, _, _) if lhs.hir_id == expr.hir_id => {
                        *state = IncrementVisitorVarState::DontWarn
                    },
                    ExprKind::AddrOf(BorrowKind::Ref, mutability, _) if mutability == Mutability::Mut => {
                        *state = IncrementVisitorVarState::DontWarn
                    },
                    _ => (),
                }
            }

            walk_expr(self, expr);
        } else if is_loop(expr) || is_conditional(expr) {
            self.depth += 1;
            walk_expr(self, expr);
            self.depth -= 1;
        } else if let ExprKind::Continue(_) = expr.kind {
            self.done = true;
        } else {
            walk_expr(self, expr);
        }
    }
    fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
        NestedVisitorMap::None
    }
}

enum InitializeVisitorState<'hir> {
    Initial,          // Not examined yet
    Declared(Symbol), // Declared but not (yet) initialized
    Initialized {
        name: Symbol,
        initializer: &'hir Expr<'hir>,
    },
    DontWarn,
}

/// Checks whether a variable is initialized at the start of a loop and not modified
/// and used after the loop.
struct InitializeVisitor<'a, 'tcx> {
    cx: &'a LateContext<'tcx>,  // context reference
    end_expr: &'tcx Expr<'tcx>, // the for loop. Stop scanning here.
    var_id: HirId,
    state: InitializeVisitorState<'tcx>,
    depth: u32, // depth of conditional expressions
    past_loop: bool,
}

impl<'a, 'tcx> InitializeVisitor<'a, 'tcx> {
    fn new(cx: &'a LateContext<'tcx>, end_expr: &'tcx Expr<'tcx>, var_id: HirId) -> Self {
        Self {
            cx,
            end_expr,
            var_id,
            state: InitializeVisitorState::Initial,
            depth: 0,
            past_loop: false,
        }
    }

    fn get_result(&self) -> Option<(Symbol, &'tcx Expr<'tcx>)> {
        if let InitializeVisitorState::Initialized { name, initializer } = self.state {
            Some((name, initializer))
        } else {
            None
        }
    }
}

impl<'a, 'tcx> Visitor<'tcx> for InitializeVisitor<'a, 'tcx> {
    type Map = Map<'tcx>;

    fn visit_stmt(&mut self, stmt: &'tcx Stmt<'_>) {
        // Look for declarations of the variable
        if_chain! {
            if let StmtKind::Local(ref local) = stmt.kind;
            if local.pat.hir_id == self.var_id;
            if let PatKind::Binding(.., ident, _) = local.pat.kind;
            then {
                self.state = local.init.map_or(InitializeVisitorState::Declared(ident.name), |init| {
                    InitializeVisitorState::Initialized {
                        initializer: init,
                        name: ident.name,
                    }
                })
            }
        }
        walk_stmt(self, stmt);
    }

    fn visit_expr(&mut self, expr: &'tcx Expr<'_>) {
        if matches!(self.state, InitializeVisitorState::DontWarn) {
            return;
        }
        if expr.hir_id == self.end_expr.hir_id {
            self.past_loop = true;
            return;
        }
        // No need to visit expressions before the variable is
        // declared
        if matches!(self.state, InitializeVisitorState::Initial) {
            return;
        }

        // If node is the desired variable, see how it's used
        if path_to_local_id(expr, self.var_id) {
            if self.past_loop {
                self.state = InitializeVisitorState::DontWarn;
                return;
            }

            if let Some(parent) = get_parent_expr(self.cx, expr) {
                match parent.kind {
                    ExprKind::AssignOp(_, ref lhs, _) if lhs.hir_id == expr.hir_id => {
                        self.state = InitializeVisitorState::DontWarn;
                    },
                    ExprKind::Assign(ref lhs, ref rhs, _) if lhs.hir_id == expr.hir_id => {
                        self.state = if_chain! {
                            if self.depth == 0;
                            if let InitializeVisitorState::Declared(name)
                                | InitializeVisitorState::Initialized { name, ..} = self.state;
                            then {
                                InitializeVisitorState::Initialized { initializer: rhs, name }
                            } else {
                                InitializeVisitorState::DontWarn
                            }
                        }
                    },
                    ExprKind::AddrOf(BorrowKind::Ref, mutability, _) if mutability == Mutability::Mut => {
                        self.state = InitializeVisitorState::DontWarn
                    },
                    _ => (),
                }
            }

            walk_expr(self, expr);
        } else if !self.past_loop && is_loop(expr) {
            self.state = InitializeVisitorState::DontWarn;
        } else if is_conditional(expr) {
            self.depth += 1;
            walk_expr(self, expr);
            self.depth -= 1;
        } else {
            walk_expr(self, expr);
        }
    }

    fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
        NestedVisitorMap::OnlyBodies(self.cx.tcx.hir())
    }
}

fn is_loop(expr: &Expr<'_>) -> bool {
    matches!(expr.kind, ExprKind::Loop(..))
}

fn is_conditional(expr: &Expr<'_>) -> bool {
    matches!(expr.kind, ExprKind::If(..) | ExprKind::Match(..))
}

fn is_nested(cx: &LateContext<'_>, match_expr: &Expr<'_>, iter_expr: &Expr<'_>) -> bool {
    if_chain! {
        if let Some(loop_block) = get_enclosing_block(cx, match_expr.hir_id);
        let parent_node = cx.tcx.hir().get_parent_node(loop_block.hir_id);
        if let Some(Node::Expr(loop_expr)) = cx.tcx.hir().find(parent_node);
        then {
            return is_loop_nested(cx, loop_expr, iter_expr)
        }
    }
    false
}

fn is_loop_nested(cx: &LateContext<'_>, loop_expr: &Expr<'_>, iter_expr: &Expr<'_>) -> bool {
    let mut id = loop_expr.hir_id;
    let iter_id = if let Some(id) = path_to_local(iter_expr) {
        id
    } else {
        return true;
    };
    loop {
        let parent = cx.tcx.hir().get_parent_node(id);
        if parent == id {
            return false;
        }
        match cx.tcx.hir().find(parent) {
            Some(Node::Expr(expr)) => {
                if let ExprKind::Loop(..) = expr.kind {
                    return true;
                };
            },
            Some(Node::Block(block)) => {
                let mut block_visitor = LoopNestVisitor {
                    hir_id: id,
                    iterator: iter_id,
                    nesting: Unknown,
                };
                walk_block(&mut block_visitor, block);
                if block_visitor.nesting == RuledOut {
                    return false;
                }
            },
            Some(Node::Stmt(_)) => (),
            _ => {
                return false;
            },
        }
        id = parent;
    }
}

#[derive(PartialEq, Eq)]
enum Nesting {
    Unknown,     // no nesting detected yet
    RuledOut,    // the iterator is initialized or assigned within scope
    LookFurther, // no nesting detected, no further walk required
}

use self::Nesting::{LookFurther, RuledOut, Unknown};

struct LoopNestVisitor {
    hir_id: HirId,
    iterator: HirId,
    nesting: Nesting,
}

impl<'tcx> Visitor<'tcx> for LoopNestVisitor {
    type Map = Map<'tcx>;

    fn visit_stmt(&mut self, stmt: &'tcx Stmt<'_>) {
        if stmt.hir_id == self.hir_id {
            self.nesting = LookFurther;
        } else if self.nesting == Unknown {
            walk_stmt(self, stmt);
        }
    }

    fn visit_expr(&mut self, expr: &'tcx Expr<'_>) {
        if self.nesting != Unknown {
            return;
        }
        if expr.hir_id == self.hir_id {
            self.nesting = LookFurther;
            return;
        }
        match expr.kind {
            ExprKind::Assign(ref path, _, _) | ExprKind::AssignOp(_, ref path, _) => {
                if path_to_local_id(path, self.iterator) {
                    self.nesting = RuledOut;
                }
            },
            _ => walk_expr(self, expr),
        }
    }

    fn visit_pat(&mut self, pat: &'tcx Pat<'_>) {
        if self.nesting != Unknown {
            return;
        }
        if let PatKind::Binding(_, id, ..) = pat.kind {
            if id == self.iterator {
                self.nesting = RuledOut;
                return;
            }
        }
        walk_pat(self, pat)
    }

    fn nested_visit_map(&mut self) -> NestedVisitorMap<Self::Map> {
        NestedVisitorMap::None
    }
}
