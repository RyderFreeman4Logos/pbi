//! Direct associated declarations from one syn parse.
//! Local functions and declarations inside an ended impl are not owners.
use syn::{
    visit::{self, Visit},
    Expr, File, ImplItem, Item, TraitItem, Type, UseTree,
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

pub(super) struct CallSite {
    pub(super) line: usize,
    pub(super) marker: String,
}

pub(super) fn calls(source: &str) -> Vec<CallSite> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut collector = CallCollector(Vec::new());
    collector.visit_file(&file);
    collector.0
}

struct CallCollector(Vec<CallSite>);

impl<'ast> Visit<'ast> for CallCollector {
    fn visit_expr_call(&mut self, call: &'ast syn::ExprCall) {
        if let Expr::Path(path) = &*call.func {
            if let Some(first) = path.path.segments.first() {
                self.0.push(CallSite {
                    line: first.ident.span().start().line,
                    marker: path
                        .path
                        .segments
                        .iter()
                        .map(|segment| segment.ident.to_string())
                        .collect::<Vec<_>>()
                        .join("::"),
                });
            }
        }
        visit::visit_expr_call(self, call);
    }

    fn visit_expr_method_call(&mut self, call: &'ast syn::ExprMethodCall) {
        self.0.push(CallSite {
            line: call.method.span().start().line,
            marker: format!(".{}", call.method),
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
