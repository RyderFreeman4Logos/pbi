//! Direct associated declarations from one syn parse.
//! Local functions and declarations inside an ended impl are not owners.
use syn::{ImplItem, Item, TraitItem, Type};

#[derive(Clone, Debug)]
pub(super) struct Declaration {
    pub(super) line: usize,
    pub(super) name: String,
    pub(super) owner: Option<String>,
}

pub(super) fn declarations(source: &str) -> Vec<Declaration> {
    let Ok(file) = syn::parse_file(source) else {
        return Vec::new();
    };
    let mut found = Vec::new();
    collect_items(&file.items, None, &mut found);
    found.sort_by_key(|declaration| declaration.line);
    found
}

fn collect_items(items: &[Item], module: Option<&str>, found: &mut Vec<Declaration>) {
    for item in items {
        match item {
            Item::Fn(function) => push(
                function.sig.ident.span(),
                &function.sig.ident,
                module,
                found,
            ),
            Item::Const(item) => push(item.ident.span(), &item.ident, module, found),
            Item::Static(item) => push(item.ident.span(), &item.ident, module, found),
            Item::Struct(item) => push(item.ident.span(), &item.ident, module, found),
            Item::Enum(item) => push(item.ident.span(), &item.ident, module, found),
            Item::Trait(item) => {
                push(item.ident.span(), &item.ident, module, found);
                for member in &item.items {
                    if let TraitItem::Fn(function) = member {
                        push(
                            function.sig.ident.span(),
                            &function.sig.ident,
                            Some(&item.ident.to_string()),
                            found,
                        );
                    }
                }
            }
            Item::Type(item) => push(item.ident.span(), &item.ident, module, found),
            Item::Mod(item) => {
                push(item.ident.span(), &item.ident, module, found);
                if let Some((_, nested)) = &item.content {
                    collect_items(nested, Some(&item.ident.to_string()), found);
                }
            }
            Item::Impl(item) => {
                let owner = impl_owner(&item.self_ty);
                for member in &item.items {
                    let (span, name) = match member {
                        ImplItem::Fn(function) => (function.sig.ident.span(), &function.sig.ident),
                        ImplItem::Const(item) => (item.ident.span(), &item.ident),
                        ImplItem::Type(item) => (item.ident.span(), &item.ident),
                        _ => continue,
                    };
                    push(span, name, owner.as_deref(), found);
                }
            }
            _ => {}
        }
    }
}

fn push(
    span: proc_macro2::Span,
    name: &syn::Ident,
    owner: Option<&str>,
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
        owner: owner.map(str::to_owned),
    });
}

fn impl_owner(ty: &Type) -> Option<String> {
    match ty {
        Type::Path(path) => path
            .path
            .segments
            .last()
            .map(|segment| segment.ident.to_string()),
        _ => None,
    }
}
