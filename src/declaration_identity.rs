//! Parsed declaration, field, and call identities from bounded Rust sources.
//! Unproven receivers and declarations inside an ended impl are not owners.
use syn::{
    visit::{self, Visit},
    Expr, File, FnArg, ImplItem, Item, Member, Pat, Stmt, TraitItem, Type, UseTree,
};

/// Segment identity, shared by lexical owners and directly resolvable impl types.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub(super) struct OwnerPath(Vec<String>);

impl OwnerPath {
    pub(super) fn is_root(&self) -> bool {
        self.0.is_empty()
    }

    fn child(&self, name: &syn::Ident) -> Self {
        let mut path = self.clone();
        path.0.push(bare(&name.to_string()).to_owned());
        path
    }

    pub(super) fn matches(&self, query: &str) -> bool {
        let requested: Vec<&str> = query.split("::").map(bare).collect();
        !requested.is_empty()
            && requested.iter().all(|segment| !segment.is_empty())
            && self.0.len() >= requested.len()
            && self
                .0
                .iter()
                .rev()
                .zip(requested.iter().rev())
                .all(|(a, b)| a == b)
    }

    pub(super) fn matches_receiver(&self, query: &str, file_stem: Option<&str>) -> bool {
        self.equals(query)
            || query.rsplit_once("::").is_some_and(|(module, ty)| {
                file_stem == Some(module) && self.0.len() == 1 && self.0[0] == bare(ty)
            })
    }

    pub(super) fn equals(&self, query: &str) -> bool {
        let requested = query.split("::").map(bare).collect::<Vec<_>>();
        requested.len() == self.0.len()
            && self
                .0
                .iter()
                .zip(requested)
                .all(|(owner, name)| owner == name)
    }

    fn query(&self) -> String {
        self.0.join("::")
    }
}

fn bare(value: &str) -> &str {
    value.strip_prefix("r#").unwrap_or(value)
}

#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) owner: Option<OwnerPath>,
}

#[derive(Clone)]
pub(super) struct CallSite {
    pub(super) line: usize,
    pub(super) marker: String,
    pub(super) receiver: Option<ReceiverPath>,
    pub(super) value_used: bool,
}

#[derive(Clone)]
pub(super) struct ReceiverPath {
    base_type: String,
    fields: Vec<String>,
}

pub(super) struct FieldType {
    owner: String,
    field: String,
    ty: String,
}

pub(super) fn receiver_owner(call: &CallSite, fields: &[FieldType]) -> Option<String> {
    let receiver = call.receiver.as_ref()?;
    let mut ty = receiver.base_type.clone();
    for field in &receiver.fields {
        let mut matches = fields
            .iter()
            .filter(|candidate| candidate.owner == ty && candidate.field == *field);
        let next = matches.next()?;
        if matches.next().is_some() {
            return None;
        }
        ty.clone_from(&next.ty);
    }
    Some(ty)
}

pub(super) fn field_types(source: &str) -> Vec<FieldType> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_fields(&file.items, "", &mut found);
    found
}

fn collect_fields(items: &[Item], module: &str, found: &mut Vec<FieldType>) {
    for item in items {
        match item {
            Item::Struct(item) => {
                let owner = format!("{module}{}", item.ident);
                for field in &item.fields {
                    if let (Some(name), Some(ty)) = (&field.ident, type_path(&field.ty)) {
                        found.push(FieldType {
                            owner: owner.clone(),
                            field: bare(&name.to_string()).to_owned(),
                            ty,
                        });
                    }
                }
            }
            Item::Mod(item) => {
                if let Some((_, nested)) = &item.content {
                    collect_fields(nested, &format!("{module}{}::", item.ident), found);
                }
            }
            _ => {}
        }
    }
}

pub(super) fn calls(source: &str) -> Vec<CallSite> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut collector = CallCollector {
        file: &file,
        module: OwnerPath::default(),
        calls: Vec::new(),
        bindings: Vec::new(),
        self_type: None,
        value_used: true,
    };
    collector.visit_file(&file);
    collector.calls
}

struct CallCollector<'a> {
    file: &'a File,
    module: OwnerPath,
    calls: Vec<CallSite>,
    bindings: Vec<(String, String)>,
    self_type: Option<String>,
    value_used: bool,
}

impl CallCollector<'_> {
    fn receiver(&self, expr: &Expr) -> Option<ReceiverPath> {
        match expr {
            Expr::Path(path) if path.path.segments.len() == 1 => {
                let name = path.path.segments.first()?.ident.to_string();
                let base_type = if name == "self" {
                    self.self_type.clone()?
                } else {
                    self.bindings
                        .iter()
                        .find(|(binding, _)| *binding == name)?
                        .1
                        .clone()
                };
                Some(ReceiverPath {
                    base_type,
                    fields: Vec::new(),
                })
            }
            Expr::Field(field) => {
                let Member::Named(name) = &field.member else {
                    return None;
                };
                let mut receiver = self.receiver(&field.base)?;
                receiver.fields.push(bare(&name.to_string()).to_owned());
                Some(receiver)
            }
            Expr::Reference(reference) => self.receiver(&reference.expr),
            Expr::Paren(paren) => self.receiver(&paren.expr),
            Expr::Group(group) => self.receiver(&group.expr),
            _ => None,
        }
    }

    fn enter_inputs(&mut self, inputs: &syn::punctuated::Punctuated<FnArg, syn::token::Comma>) {
        self.bindings = inputs
            .iter()
            .filter_map(|arg| {
                let FnArg::Typed(arg) = arg else { return None };
                let Pat::Ident(name) = &*arg.pat else {
                    return None;
                };
                Some((name.ident.to_string(), type_path(&arg.ty)?))
            })
            .collect();
    }
}

fn type_path(ty: &Type) -> Option<String> {
    match ty {
        Type::Reference(reference) => type_path(&reference.elem),
        Type::Paren(paren) => type_path(&paren.elem),
        Type::Group(group) => type_path(&group.elem),
        Type::Path(path)
            if path.qself.is_none()
                && path
                    .path
                    .segments
                    .iter()
                    .all(|segment| matches!(segment.arguments, syn::PathArguments::None)) =>
        {
            Some(
                path.path
                    .segments
                    .iter()
                    .map(|segment| bare(&segment.ident.to_string()).to_owned())
                    .collect::<Vec<_>>()
                    .join("::"),
            )
        }
        _ => None,
    }
}

impl<'ast> Visit<'ast> for CallCollector<'_> {
    fn visit_item_mod(&mut self, item: &'ast syn::ItemMod) {
        let previous = self.module.clone();
        self.module = self.module.child(&item.ident);
        visit::visit_item_mod(self, item);
        self.module = previous;
    }

    fn visit_item_fn(&mut self, function: &'ast syn::ItemFn) {
        let previous = std::mem::take(&mut self.bindings);
        let self_type = self.self_type.take();
        self.enter_inputs(&function.sig.inputs);
        visit::visit_item_fn(self, function);
        self.bindings = previous;
        self.self_type = self_type;
    }

    fn visit_item_impl(&mut self, item: &'ast syn::ItemImpl) {
        let owner = impl_owner(
            self.file,
            &item.self_ty,
            &self.module,
            &impl_binders(&item.generics),
        )
        .map(|owner| owner.query());
        let previous = std::mem::replace(&mut self.self_type, owner);
        visit::visit_item_impl(self, item);
        self.self_type = previous;
    }

    fn visit_impl_item_fn(&mut self, function: &'ast syn::ImplItemFn) {
        let previous = std::mem::take(&mut self.bindings);
        self.enter_inputs(&function.sig.inputs);
        visit::visit_impl_item_fn(self, function);
        self.bindings = previous;
    }

    fn visit_stmt(&mut self, statement: &'ast Stmt) {
        let previous = self.value_used;
        self.value_used = !matches!(
            statement,
            Stmt::Expr(
                Expr::Call(_) | Expr::MethodCall(_) | Expr::Await(_),
                Some(_)
            )
        );
        visit::visit_stmt(self, statement);
        self.value_used = previous;
    }

    fn visit_local(&mut self, local: &'ast syn::Local) {
        visit::visit_local(self, local);
        if let Pat::Ident(name) = &local.pat {
            self.bindings
                .retain(|(binding, _)| binding != &name.ident.to_string());
        }
    }

    fn visit_expr_closure(&mut self, closure: &'ast syn::ExprClosure) {
        let previous = std::mem::take(&mut self.bindings);
        let self_type = self.self_type.take();
        visit::visit_expr_closure(self, closure);
        self.bindings = previous;
        self.self_type = self_type;
    }

    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = &*call.func {
            if let Some(first) = path.path.segments.first() {
                self.calls.push(CallSite {
                    line: first.ident.span().start().line,
                    marker: path
                        .path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::"),
                    receiver: None,
                    value_used: self.value_used,
                });
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.calls.push(CallSite {
            line: call.method.span().start().line,
            marker: format!(".{}", call.method),
            receiver: self.receiver(&call.receiver),
            value_used: self.value_used,
        });
        visit::visit_expr_method_call(self, call);
    }
}

pub(super) fn declarations(source: &str) -> Vec<Declaration> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_items(&file, &file.items, &OwnerPath::default(), &mut found);
    found.sort_by_key(|declaration| declaration.line);
    found
}

fn collect_items(file: &File, items: &[Item], module: &OwnerPath, found: &mut Vec<Declaration>) {
    for item in items {
        match item {
            Item::Fn(function) => push(
                function.sig.ident.span(),
                &function.sig.ident,
                Some(module),
                found,
            ),
            Item::Const(item) => push(item.ident.span(), &item.ident, Some(module), found),
            Item::Static(item) => push(item.ident.span(), &item.ident, Some(module), found),
            Item::Struct(item) => push(item.ident.span(), &item.ident, Some(module), found),
            Item::Enum(item) => push(item.ident.span(), &item.ident, Some(module), found),
            Item::Trait(item) => {
                push(item.ident.span(), &item.ident, Some(module), found);
                let owner = module.child(&item.ident);
                for member in &item.items {
                    let (span, name) = match member {
                        TraitItem::Fn(function) => (function.sig.ident.span(), &function.sig.ident),
                        TraitItem::Const(item) => (item.ident.span(), &item.ident),
                        TraitItem::Type(item) => (item.ident.span(), &item.ident),
                        _ => continue,
                    };
                    push(span, name, Some(&owner), found);
                }
            }
            Item::Type(item) => push(item.ident.span(), &item.ident, Some(module), found),
            Item::Mod(item) => {
                push(item.ident.span(), &item.ident, Some(module), found);
                if let Some((_, nested)) = &item.content {
                    let nested_module = module.child(&item.ident);
                    collect_items(file, nested, &nested_module, found);
                }
            }
            Item::Impl(item) => {
                let binders = impl_binders(&item.generics);
                let owner = impl_owner(file, &item.self_ty, module, &binders);
                for member in &item.items {
                    let (span, name) = match member {
                        ImplItem::Fn(function) => (function.sig.ident.span(), &function.sig.ident),
                        ImplItem::Const(item) => (item.ident.span(), &item.ident),
                        ImplItem::Type(item) => (item.ident.span(), &item.ident),
                        _ => continue,
                    };
                    push(span, name, owner.as_ref(), found);
                }
            }
            _ => {}
        }
    }
}

fn push(
    span: proc_macro2::Span,
    name: &syn::Ident,
    owner: Option<&OwnerPath>,
    found: &mut Vec<Declaration>,
) {
    let line = span.start().line;
    if line == 0 {
        return;
    }
    let declared = name.to_string();
    let name = declared.strip_prefix("r#").unwrap_or(&declared).to_owned();
    found.push(Declaration {
        line,
        name,
        owner: owner.cloned(),
    });
}

fn impl_binders(generics: &syn::Generics) -> Vec<String> {
    generics
        .params
        .iter()
        .filter_map(|param| match param {
            syn::GenericParam::Type(param) => Some(bare(&param.ident.to_string()).to_owned()),
            _ => None,
        })
        .collect()
}

fn proven_root(items: &[Item], module: &[String], root: &str) -> bool {
    if module.is_empty() {
        return items.iter().any(|item| item_binds(item, root));
    }
    items.iter().any(|item| match item {
        Item::Mod(item) if bare(&item.ident.to_string()) == module[0] => item
            .content
            .as_ref()
            .is_some_and(|(_, nested)| proven_root(nested, &module[1..], root)),
        _ => false,
    })
}

fn item_binds(item: &Item, root: &str) -> bool {
    match item {
        Item::Use(item) => use_binds(&item.tree, root),
        Item::Struct(item) => bare(&item.ident.to_string()) == root,
        Item::Enum(item) => bare(&item.ident.to_string()) == root,
        Item::Trait(item) => bare(&item.ident.to_string()) == root,
        Item::Type(item) => bare(&item.ident.to_string()) == root,
        Item::Union(item) => bare(&item.ident.to_string()) == root,
        Item::Mod(item) => bare(&item.ident.to_string()) == root,
        _ => false,
    }
}

fn use_binds(tree: &UseTree, root: &str) -> bool {
    match tree {
        UseTree::Path(path) => path_binds(&path.ident, &path.tree, root),
        UseTree::Group(group) => group.items.iter().any(|item| use_binds(item, root)),
        _ => bound_name(tree).is_some_and(|name| name == root),
    }
}

fn path_binds(parent: &syn::Ident, tree: &UseTree, root: &str) -> bool {
    match tree {
        UseTree::Name(name) if name.ident == "self" => bare(&parent.to_string()) == root,
        UseTree::Rename(name) if name.ident == "self" => bare(&name.rename.to_string()) == root,
        UseTree::Group(group) => group
            .items
            .iter()
            .any(|item| path_binds(parent, item, root)),
        other => use_binds(other, root),
    }
}

fn bound_name(tree: &UseTree) -> Option<String> {
    match tree {
        UseTree::Name(name) => Some(bare(&name.ident.to_string()).to_owned()),
        UseTree::Rename(name) => Some(bare(&name.rename.to_string()).to_owned()),
        UseTree::Glob(_) => None,
        UseTree::Path(path) => bound_name(&path.tree),
        UseTree::Group(_) => None,
    }
}

fn impl_owner(file: &File, ty: &Type, module: &OwnerPath, binders: &[String]) -> Option<OwnerPath> {
    let Type::Path(ty) = ty else { return None };
    // Unproven and generic roots have no concrete owner. Do not invent a module.
    if ty.qself.is_some() || ty.path.leading_colon.is_some() {
        return None;
    }
    let mut segments = ty.path.segments.iter().peekable();
    let mut owner = module.clone();
    let mut rooted = false;
    match segments.peek()?.ident.to_string().as_str() {
        "crate" => {
            owner = OwnerPath::default();
            rooted = true;
            segments.next();
        }
        "self" => {
            rooted = true;
            segments.next();
        }
        "super" => {
            while segments
                .peek()
                .is_some_and(|segment| segment.ident == "super")
            {
                owner.0.pop()?;
                segments.next();
            }
            rooted = true;
        }
        _ => {}
    }
    let mut has_type = false;
    let segments: Vec<_> = segments.collect();
    for segment in &segments {
        let name = bare(&segment.ident.to_string()).to_owned();
        if matches!(name.as_str(), "crate" | "self" | "super" | "Self") {
            return None;
        }
        if !has_type && !rooted {
            if binders.iter().any(|binder| binder == &name) {
                return None;
            }
            if !owner.0.is_empty() && !proven_root(&file.items, &owner.0, &name) {
                return None;
            }
        }
        owner.0.push(name);
        has_type = true;
    }
    has_type.then_some(owner)
}
