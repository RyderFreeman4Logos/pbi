use super::*;

fn expect_owner_miss(source: &str, queries: &[&str]) {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", source);
    for query in queries {
        expect_miss(&fixture.root, &output, query);
    }
}

fn expect_owner_hit(source: &str, query: &str, line: usize, name: &str, owner: &str) {
    let fixture = Fixture::new();
    let output = fixture.write("src/lib.rs", source);
    expect_path_identity(&fixture.root, &output, query, line, name, Some(owner));
}

#[test]
fn definition_self_type_root_requires_proven_local_binding() {
    expect_owner_miss(
        "pub mod elsewhere {\n pub trait Marker { fn target_func(&self); }\n impl Marker for std::string::String {\n  fn target_func(&self) {}\n }\n}\n",
        &[
            "where is elsewhere::std::string::String::target_func defined",
            "where is wrong::std::string::String::target_func defined",
        ],
    );
    expect_owner_miss(
        "pub mod elsewhere {\n pub trait Marker { fn target_func(&self); }\n impl Marker for vendor::foreign::Widget {\n  fn target_func(&self) {}\n }\n}\n",
        &["where is elsewhere::vendor::foreign::Widget::target_func defined"],
    );
    expect_owner_hit(
        "pub mod elsewhere {\n pub mod local {\n  pub struct Owner;\n }\n impl local::Owner {\n  pub fn target_func(&self) {}\n }\n}\n",
        "where is elsewhere::local::Owner::target_func defined",
        6,
        "target_func",
        "elsewhere::local::Owner",
    );
}

#[test]
fn definition_self_type_generic_binder_shadows_same_name_struct() {
    expect_owner_miss(
        "pub mod elsewhere {\n pub struct Owner;\n pub trait Bound {}\n pub trait Marker { fn target_func(&self); }\n impl<Owner: Bound> Marker for Owner {\n  fn target_func(&self) {}\n }\n}\n",
        &[
            "where is elsewhere::Owner::target_func defined",
            "where is Owner::target_func defined",
        ],
    );
}

#[test]
fn definition_self_type_import_alias_keeps_path_identity() {
    let source = "pub mod actual {\n pub struct Owner;\n}\npub mod elsewhere {\n use crate::actual as renamed;\n impl renamed::Owner {\n  pub fn target_func(&self) {}\n }\n}\n";
    expect_owner_hit(
        source,
        "where is elsewhere::renamed::Owner::target_func defined",
        7,
        "target_func",
        "elsewhere::renamed::Owner",
    );
    expect_owner_miss(
        source,
        &[
            "where is actual::Owner::target_func defined",
            "where is elsewhere::actual::Owner::target_func defined",
        ],
    );
}
