//! Direct associated declarations from one syn parse.
//! Local functions and declarations inside an ended impl are not owners.
use syn::{ImplItem, Item, TraitItem, Type};

/// Segment identity, shared by lexical owners and directly resolvable impl types.
#[derive(Clone, Debug, Default)]
pub(super) struct OwnerPath(Vec<String>);

impl OwnerPath {
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

pub(super) fn declarations(source: &str) -> Vec<Declaration> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_items(&file.items, &OwnerPath::default(), &mut found);
    found.sort_by_key(|declaration| declaration.line);
    found
}

fn collect_items(items: &[Item], module: &OwnerPath, found: &mut Vec<Declaration>) {
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
                    collect_items(nested, &nested_module, found);
                }
            }
            Item::Impl(item) => {
                let owner = impl_owner(&item.self_ty, module);
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

fn impl_owner(ty: &Type, module: &OwnerPath) -> Option<OwnerPath> {
    let Type::Path(ty) = ty else { return None };
    // No inference of projections, extern-prelude roots, imports or aliases.
    // Preserve explicit identity; never substitute the impl's lexical module.
    if ty.qself.is_some() || ty.path.leading_colon.is_some() {
        return None;
    }
    let mut segments = ty.path.segments.iter().peekable();
    let mut owner = module.clone();
    match segments.peek()?.ident.to_string().as_str() {
        "crate" => {
            owner = OwnerPath::default();
            segments.next();
        }
        "self" => {
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
        }
        _ => {}
    }
    let mut has_type = false;
    for segment in segments {
        let name = segment.ident.to_string();
        if matches!(name.as_str(), "crate" | "self" | "super" | "Self") {
            return None;
        }
        owner.0.push(bare(&name).to_owned());
        has_type = true;
    }
    has_type.then_some(owner)
}
