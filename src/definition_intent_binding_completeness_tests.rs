use super::*;

fn grouped(import: &str, local: &str) -> String {
    format!(
        "pub mod actual {{ pub struct Other; pub struct Owner; }}\npub mod elsewhere {{\n{import}\nimpl {local} {{\npub fn target_func(&self) {{}}\n}}\n}}\n"
    )
}

fn expect_grouped(import: &str, local: &str, owner: &str) {
    let source = grouped(import, local);
    expect_owner_hit(
        &source,
        &format!("where is {owner}::target_func defined"),
        5,
        "target_func",
        owner,
    );
    expect_owner_miss(
        &source,
        &["where is elsewhere::wrong::Owner::target_func defined"],
    );
}

#[test]
fn definition_group_binds_every_sibling_not_only_the_first() {
    for (import, local, owner) in [
        (
            "use crate::actual::{Other, Owner};",
            "Owner",
            "elsewhere::Owner",
        ),
        (
            "use crate::actual::{Owner, Other};",
            "Owner",
            "elsewhere::Owner",
        ),
        (
            "use crate::actual::{self, Owner};",
            "Owner",
            "elsewhere::Owner",
        ),
        (
            "use crate::actual::{Owner, self};",
            "actual::Owner",
            "elsewhere::actual::Owner",
        ),
        (
            "use crate::actual::{Other, Owner as Alias};",
            "Alias",
            "elsewhere::Alias",
        ),
        (
            "use crate::actual::{Owner, self as alias};",
            "alias::Owner",
            "elsewhere::alias::Owner",
        ),
        (
            "use crate::{actual::{Other, Owner}};",
            "Owner",
            "elsewhere::Owner",
        ),
        (
            "use crate::actual::{Other, r#type as raw};",
            "raw",
            "elsewhere::raw",
        ),
    ] {
        expect_grouped(import, local, owner);
    }
}

#[test]
fn definition_proven_root_binds_local_union_not_a_foreign_owner() {
    expect_owner_hit(
        "pub mod elsewhere {\npub union Owner { pub value: u32 }\nimpl Owner {\npub fn target_func(&self) {}\n}\n}\n",
        "where is elsewhere::Owner::target_func defined",
        4,
        "target_func",
        "elsewhere::Owner",
    );
    expect_owner_miss(
        "pub mod elsewhere {\npub union Other { pub value: u32 }\nimpl Owner {\npub fn target_func(&self) {}\n}\n}\n",
        &["where is elsewhere::Owner::target_func defined"],
    );
}
