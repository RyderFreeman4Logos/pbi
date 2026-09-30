//! Bounded syntactic witnesses, not type checking or general data-flow analysis.
//! Parse once; require the displayed window to include both decision and action.
use proc_macro2::Span;
use std::collections::{BTreeMap, BTreeSet};
use syn::{
    spanned::Spanned,
    visit::{self, Visit},
    BinOp, Block, Expr, FnArg, Item, Lit, Pat, Stmt,
};

const MAX_ANALYSIS_BYTES: usize = 256 * 1024;
const MAX_DEPTH: usize = 64;
const MAX_EXPRESSION_BYTES: usize = 256;

#[derive(Default)]
pub(super) struct Proofs(Vec<(usize, usize)>);
impl Proofs {
    pub(super) fn new(source: &str, code: &str) -> Self {
        // Source admission remains 2 MiB. Structural analysis has a smaller hard
        // budget, including recursion/long-expression limits before syn parsing.
        if !bounded(code) {
            return Self::default();
        }
        let Ok(file) = syn::parse_file(source) else {
            return Self::default();
        };
        let mut predicates = BTreeMap::new();
        let mut transfers = BTreeMap::new();
        for item in &file.items {
            if let Item::Fn(function) = item {
                let name = function.sig.ident.to_string();
                let parameter = function.sig.inputs.first().and_then(|arg| match arg {
                    FnArg::Typed(arg) => match &*arg.pat {
                        Pat::Ident(p) => Some(p.ident.to_string()),
                        _ => None,
                    },
                    _ => None,
                });
                let proved = parameter.is_some_and(|p| {
                    function.sig.inputs.len() == 1
                        && function.block.stmts.len() == 1
                        && returned(&function.block.stmts[0])
                            .is_some_and(|e| literal_comparison(e, &p, false))
                });
                let transfer = function
                    .sig
                    .inputs
                    .iter()
                    .take(if function.sig.inputs.len() <= 8 { 8 } else { 0 })
                    .enumerate()
                    .find_map(|(index, arg)| {
                        let FnArg::Typed(arg) = arg else {
                            return None;
                        };
                        let Pat::Ident(p) = &*arg.pat else {
                            return None;
                        };
                        template_return(&function.block, &p.ident.to_string()).then_some(index)
                    });
                transfers
                    .entry(name.clone())
                    .and_modify(|value| *value = None)
                    .or_insert(transfer);
                predicates
                    .entry(name)
                    .and_modify(|value| *value = false)
                    .or_insert(proved);
            }
        }
        let mut collector = Collector {
            proofs: Self::default(),
            predicates: &predicates,
            transfers: &transfers,
            returned: false,
            shadowed: BTreeSet::new(),
        };
        collector.visit_file(&file);
        collector.proofs.0.sort_unstable();
        collector.proofs.0.dedup();
        collector.proofs
    }
    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    pub(super) fn covers(&self, start: usize, end: usize) -> bool {
        // Sorted decision offsets: no per-window whole-file literal scan.
        let first = self.0.partition_point(|(line, _)| *line < start);
        self.0[first..]
            .iter()
            .take_while(|(line, _)| *line <= end)
            .any(|(_, last)| *last <= end)
    }
    fn add(&mut self, decision: Span, action: Span) {
        let start = decision.start().line;
        let end = action.end().line;
        if start > 0 && end >= start && end - start < super::MAX_EVIDENCE_LINES {
            self.0.push((start, end));
        }
    }
}

fn bounded(code: &str) -> bool {
    if code.len() > MAX_ANALYSIS_BYTES {
        return false;
    }
    let (mut depth, mut run) = (0usize, 0usize);
    for byte in code.bytes() {
        match byte {
            b'(' | b'[' | b'{' => {
                depth += 1;
                if depth > MAX_DEPTH {
                    return false;
                }
            }
            b')' | b']' | b'}' => {
                depth = depth.saturating_sub(1);
            }
            _ => {}
        }
        if matches!(byte, b';' | b',' | b'{' | b'}') {
            run = 0;
        } else if !byte.is_ascii_whitespace() {
            run += 1;
        }
        // Counting bytes overestimates token count, bounding expression recursion.
        if run > MAX_EXPRESSION_BYTES {
            return false;
        }
    }
    depth == 0
}

fn bare(expr: &Expr) -> &Expr {
    match expr {
        Expr::Paren(e) => bare(&e.expr),
        Expr::Group(e) => bare(&e.expr),
        _ => expr,
    }
}
fn name(expr: &Expr) -> Option<String> {
    match bare(expr) {
        Expr::Path(e) if e.qself.is_none() => Some(
            e.path
                .segments
                .iter()
                .map(|s| s.ident.to_string())
                .collect::<Vec<_>>()
                .join("::"),
        ),
        _ => None,
    }
}
fn key(expr: &Expr) -> Option<String> {
    match bare(expr) {
        Expr::Reference(e) => key(&e.expr),
        Expr::MethodCall(e)
            if e.args.is_empty()
                && matches!(
                    e.method.to_string().as_str(),
                    "clone" | "to_owned" | "as_str"
                ) =>
        {
            key(&e.receiver)
        }
        e => name(e),
    }
}
fn carries(
    expr: &Expr,
    expected: &str,
    transfers: &BTreeMap<String, Option<usize>>,
    shadowed: &BTreeSet<String>,
) -> bool {
    match bare(expr) {
        Expr::Tuple(e) => e
            .elems
            .iter()
            .any(|e| carries(e, expected, transfers, shadowed)),
        Expr::Try(e) => carries(&e.expr, expected, transfers, shadowed),
        Expr::Call(e) => name(&e.func)
            .filter(|n| !shadowed.contains(n))
            .and_then(|n| transfers.get(&n))
            .copied()
            .flatten()
            .and_then(|index| e.args.get(index))
            .is_some_and(|e| key(e).as_deref() == Some(expected)),
        e => key(e).is_some_and(|k| k == expected),
    }
}
fn returned(stmt: &Stmt) -> Option<&Expr> {
    match stmt {
        Stmt::Expr(Expr::Return(e), _) => e.expr.as_deref(),
        Stmt::Expr(e, None) => Some(e),
        _ => None,
    }
}
fn literal_comparison(expr: &Expr, parameter: &str, rejection: bool) -> bool {
    let Expr::Binary(e) = bare(expr) else {
        return false;
    };
    let operator = matches!(e.op, BinOp::Eq(_)) || (rejection && matches!(e.op, BinOp::Ne(_)));
    operator
        && key(&e.left).is_some_and(|k| k == parameter)
        && matches!(bare(&e.right), Expr::Lit(e) if matches!(e.lit, Lit::Str(_)))
}
fn diagnostic(expr: &Expr, kind: &str) -> bool {
    match bare(expr) {
        Expr::Lit(e) => match &e.lit {
            Lit::Str(s) => {
                let tokens = super::tokenized(&s.value());
                tokens.iter().any(|t| t == kind)
                    && tokens.iter().any(|t| t == "field" || t == "fields")
            }
            _ => false,
        },
        Expr::Call(e) => name(&e.func).is_some_and(|n| {
            let tokens = super::tokenized(&n);
            tokens.iter().any(|t| t == kind) && tokens.iter().any(|t| t == "field" || t == "fields")
        }),
        _ => false,
    }
}
fn rejection(expr: &Expr) -> bool {
    match bare(expr) {
        Expr::Return(e) => e.expr.as_deref().is_some_and(rejection),
        Expr::Call(e) if name(&e.func).is_some_and(|n| n == "Err") && e.args.len() == 1 => {
            diagnostic(&e.args[0], "unknown")
        }
        _ => false,
    }
}

struct Collector<'a> {
    proofs: Proofs,
    predicates: &'a BTreeMap<String, bool>,
    transfers: &'a BTreeMap<String, Option<usize>>,
    returned: bool,
    shadowed: BTreeSet<String>,
}
impl Collector<'_> {
    fn capture(&mut self, expr: &syn::ExprIf) {
        let (condition, negative) = match bare(&expr.cond) {
            Expr::Unary(e) if matches!(e.op, syn::UnOp::Not(_)) => (bare(&e.expr), true),
            // A conjunctive membership guard still has a value-bearing else
            // fallback. Do not infer negated/disjunctive membership decisions.
            Expr::Binary(e) if matches!(e.op, BinOp::And(_)) => (bare(&e.right), false),
            e => (e, false),
        };
        let (receiver, argument) = match condition {
            Expr::MethodCall(e) if e.method == "contains" && e.args.len() == 1 => {
                let Some(receiver) = name(&e.receiver) else {
                    return;
                };
                if !super::tokenized(&receiver)
                    .iter()
                    .any(|t| matches!(t.as_str(), "schema" | "field" | "fields"))
                {
                    return;
                }
                (Some(receiver), &e.args[0])
            }
            Expr::Call(e)
                if e.args.len() == 1
                    && name(&e.func).is_some_and(|n| {
                        !self.shadowed.contains(&n) && self.predicates.get(&n) == Some(&true)
                    }) =>
            {
                (None, &e.args[0])
            }
            _ => return,
        };
        let Some(argument) = key(argument) else {
            return;
        };
        let mut history = History {
            receiver: receiver.as_deref(),
            key: &argument,
            invalid: false,
        };
        history.visit_expr_if(expr);
        if history.invalid {
            return;
        }
        let branch = if negative {
            &expr.then_branch
        } else {
            let Some((_, e)) = &expr.else_branch else {
                return;
            };
            let Expr::Block(e) = bare(e) else {
                return;
            };
            &e.block
        };
        // Only direct statements: conditional side effects / block arguments do
        // not establish retained values or an unconditional fallback action.
        for stmt in &branch.stmts {
            if let Stmt::Expr(Expr::MethodCall(call), _) = stmt {
                if matches!(call.method.to_string().as_str(), "push" | "insert")
                    && call
                        .args
                        .iter()
                        .any(|e| carries(e, &argument, self.transfers, &self.shadowed))
                {
                    self.proofs.add(expr.if_token.span, call.span());
                }
            }
        }
    }
}
impl<'ast> Visit<'ast> for Collector<'_> {
    fn visit_expr(&mut self, expr: &'ast Expr) {
        let saved = self.returned;
        // Value context flows only through transparent containers; operands of
        // calls/unwraps/assignments cannot borrow their parent's return authority.
        self.returned = matches!(expr, Expr::Try(_))
            || (saved
                && matches!(
                    expr,
                    Expr::If(_)
                        | Expr::Match(_)
                        | Expr::Block(_)
                        | Expr::Paren(_)
                        | Expr::Group(_)
                        | Expr::Return(_)
                ));
        visit::visit_expr(self, expr);
        self.returned = saved;
    }

    fn visit_item_mod(&mut self, _: &'ast syn::ItemMod) {
        // Module-local resolution is deliberately unsupported, never borrow a
        // same-named predicate from another module.
    }
    fn visit_item_fn(&mut self, item: &'ast syn::ItemFn) {
        let saved = self.returned;
        self.returned = true;
        let mut bindings = Bindings::default();
        bindings.visit_signature(&item.sig);
        bindings.visit_block(&item.block);
        let previous = std::mem::replace(&mut self.shadowed, bindings.0);
        self.visit_block(&item.block);
        self.shadowed = previous;
        self.returned = saved;
    }
    fn visit_impl_item_fn(&mut self, item: &'ast syn::ImplItemFn) {
        let saved = self.returned;
        self.returned = true;
        let mut bindings = Bindings::default();
        bindings.visit_signature(&item.sig);
        bindings.visit_block(&item.block);
        let previous = std::mem::replace(&mut self.shadowed, bindings.0);
        self.visit_block(&item.block);
        self.shadowed = previous;
        self.returned = saved;
    }
    fn visit_block(&mut self, block: &'ast Block) {
        let saved = self.returned;
        for (index, stmt) in block.stmts.iter().enumerate() {
            self.returned =
                saved && index + 1 == block.stmts.len() && matches!(stmt, Stmt::Expr(_, None));
            self.visit_stmt(stmt);
        }
        self.returned = saved;
    }
    fn visit_expr_return(&mut self, expr: &'ast syn::ExprReturn) {
        let saved = self.returned;
        self.returned = true;
        visit::visit_expr_return(self, expr);
        self.returned = saved;
    }
    fn visit_expr_if(&mut self, expr: &'ast syn::ExprIf) {
        self.capture(expr);
        if let Expr::Binary(condition) = bare(&expr.cond) {
            if key(&condition.left).is_some_and(|k| literal_comparison(&expr.cond, &k, true)) {
                for (index, stmt) in expr.then_branch.stmts.iter().enumerate() {
                    let exits = matches!(stmt, Stmt::Expr(Expr::Return(_), _))
                        || (self.returned && index + 1 == expr.then_branch.stmts.len());
                    if exits && returned(stmt).is_some_and(rejection) {
                        self.proofs.add(expr.if_token.span, stmt.span());
                    }
                }
            }
        }
        visit::visit_expr_if(self, expr);
    }
    fn visit_expr_match(&mut self, expr: &'ast syn::ExprMatch) {
        if self.returned
            && key(&expr.expr).is_some()
            && expr.arms.iter().any(|arm| matches!(arm.pat, Pat::Lit(_)))
        {
            for arm in &expr.arms {
                if arm.guard.is_none()
                    && matches!(arm.pat, Pat::Ident(_) | Pat::Wild(_))
                    && rejection(&arm.body)
                {
                    self.proofs.add(expr.match_token.span, arm.body.span());
                }
            }
        }
        visit::visit_expr_match(self, expr);
    }
    fn visit_item_struct(&mut self, item: &'ast syn::ItemStruct) {
        self.serde(&item.attrs, item.span());
    }
    fn visit_item_enum(&mut self, item: &'ast syn::ItemEnum) {
        self.serde(&item.attrs, item.span());
    }
}
impl Collector<'_> {
    fn serde(&mut self, attrs: &[syn::Attribute], span: Span) {
        let mut deserialize = false;
        let mut deny = false;
        for attr in attrs {
            if attr.path().is_ident("derive") {
                if let Ok(paths) = attr.parse_args_with(
                    syn::punctuated::Punctuated::<syn::Path, syn::Token![,]>::parse_terminated,
                ) {
                    deserialize |= paths
                        .iter()
                        .any(|p| p.segments.last().is_some_and(|s| s.ident == "Deserialize"));
                }
            }
            if attr.path().is_ident("serde") {
                let _ = attr.parse_nested_meta(|meta| {
                    deny |= meta.path.is_ident("deny_unknown_fields");
                    Ok(())
                });
            }
        }
        if deserialize && deny {
            self.proofs.add(span, span);
        }
    }
}
struct History<'a> {
    receiver: Option<&'a str>,
    key: &'a str,
    invalid: bool,
}
impl<'ast> Visit<'ast> for History<'_> {
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        if pat.ident == self.key || self.receiver.is_some_and(|r| pat.ident == r) {
            self.invalid = true;
        }
        visit::visit_pat_ident(self, pat);
    }
    fn visit_macro(&mut self, _: &'ast syn::Macro) {
        // An opaque macro may mutate the membership receiver. No expansion proof.
        self.invalid = true;
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        if self
            .receiver
            .is_some_and(|r| name(&call.receiver).as_deref() == Some(r))
            && call.method != "contains"
        {
            self.invalid = true;
        }
        visit::visit_expr_method_call(self, call);
    }
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if name(&call.func).is_some_and(|n| n == "Err")
            && call.args.iter().any(|e| diagnostic(e, "duplicate"))
        {
            self.invalid = true;
        }
        visit::visit_expr_call(self, call);
    }
}

// Minimal local helper summary: an Ok(local) whose unique immutable initializer
// is a match producing format strings carrying the parameter in every arm.
// No arbitrary call argument occurrence is accepted as returned-value linkage.
fn template_return(block: &Block, parameter: &str) -> bool {
    let Some(Stmt::Expr(Expr::Call(tail), None)) = block.stmts.last() else {
        return false;
    };
    if name(&tail.func).as_deref() != Some("Ok") || tail.args.len() != 1 {
        return false;
    }
    let Some(output) = name(&tail.args[0]) else {
        return false;
    };
    let mut linked = false;
    for stmt in &block.stmts[..block.stmts.len() - 1] {
        match stmt {
            Stmt::Local(local) => {
                let Pat::Ident(binding) = &local.pat else {
                    return false;
                };
                if binding.mutability.is_some()
                    || binding.ident == parameter
                    || binding.ident != output
                    || linked
                {
                    return false;
                }
                let Some(init) = &local.init else {
                    return false;
                };
                if init.diverge.is_some() {
                    return false;
                }
                let Expr::Match(value) = bare(&init.expr) else {
                    return false;
                };
                linked = !value.arms.is_empty()
                    && value.arms.iter().all(|arm| {
                        if arm.guard.is_some() {
                            return false;
                        }
                        if matches!(&arm.pat, Pat::Ident(p) if p.ident == parameter) {
                            return false;
                        }
                        let Expr::Macro(expr) = bare(&arm.body) else {
                            return false;
                        };
                        if !expr.mac.path.is_ident("format") {
                            return false;
                        }
                        let Ok(format) = syn::parse2::<syn::LitStr>(expr.mac.tokens.clone()) else {
                            return false;
                        };
                        let text = format.value();
                        // Escaped braces cannot establish an interpolation witness.
                        !text.contains("{{")
                            && !text.contains("}}")
                            && text.contains(&format!("{{{parameter}}}"))
                    });
            }
            Stmt::Expr(Expr::If(guard), _) if guard.else_branch.is_none() => {
                // Early error exits may reject a key, but cannot rewrite it or
                // supply an unrelated successful return value.
                if !guard.then_branch.stmts.iter().all(|s| matches!(s, Stmt::Expr(Expr::Return(e), _) if e.expr.as_deref().is_some_and(|e| matches!(bare(e), Expr::Call(e) if name(&e.func).as_deref() == Some("Err"))))) { return false; }
            }
            _ => return false,
        }
    }
    linked
}

#[derive(Default)]
struct Bindings(BTreeSet<String>);
impl<'ast> Visit<'ast> for Bindings {
    fn visit_pat_ident(&mut self, pat: &'ast syn::PatIdent) {
        self.0.insert(pat.ident.to_string());
        visit::visit_pat_ident(self, pat);
    }
    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        self.0.insert(function.sig.ident.to_string());
    }
}
