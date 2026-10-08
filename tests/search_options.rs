use std::ffi::OsString;
use std::fs;
use std::os::unix::ffi::OsStringExt;
use std::os::unix::fs::symlink;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Command, Output};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = safe_test_root("search-options");
        fs::create_dir_all(&root).expect("create fixture root");
        fs::write(
            root.join("fixture.rs"),
            "fn search_option() {}\nfn parity() {}\n",
        )
        .expect("write fixture source");
        Self { root }
    }

    fn run(&self, args: &[&str], _mode: &str) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .env("PBI_RS_ADK_ENABLE", "0")
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run pbi-rs")
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

#[test]
fn compact_streaming_keeps_strong_late_body_beyond_raw_block_cap() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("tasks"),
        "orbit\n\n\n\n\n\n\n\n".repeat(128),
    )
    .expect("admitted raw block boundary");
    let raw = fixture.run(&["search", "--bm25", "orbit", "--max-results=2"], "raw");
    assert!(raw.status.success());
    assert!(raw.stderr.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&raw.stdout)
            .matches("File: ")
            .count(),
        2
    );
    for count in [128, 129, 512, 2048] {
        let mut source = "orbit\n\n\n\n\n\n\n\n".repeat(count);
        source.push_str("orbit vector orbit vector\norbit vector\n");
        fs::write(fixture.root.join("tasks"), source).expect("separated blocks");
        let output = fixture.run(&["search", "orbit vector"], "compact");
        assert!(output.status.success(), "compact must scan beyond block128");
        assert_eq!(
            compact_locations(&output),
            [format!("tasks:{}-{}", count * 8 + 1, count * 8 + 2)]
        );
        let raw = fixture.run(
            &["search", "--bm25", "orbit vector", "--max-results=2"],
            "raw",
        );
        assert!(
            raw.status.success(),
            "raw ranks the dense body inside the cap"
        );
        let stdout = String::from_utf8_lossy(&raw.stdout);
        assert!(stdout.contains(&format!("{}-{}", count * 8 + 1, count * 8 + 2)));
        assert!(stdout.matches("File: ").count() <= 2);
    }
}

#[test]
fn named_symbol_sentence_punctuation_keeps_declaration_priority() {
    let fixture = Fixture::new();
    let noise = format!(
        "fn noise() {{\n{}}}\n",
        "    let _ = \"SourceLocation\";\n".repeat(100)
    );
    fs::write(fixture.root.join("a_noise.rs"), noise).expect("string-only distractor");
    let declaration = format!(
        "pub struct SourceLocation;\nfn padding() {{\n{}}}\n",
        "    let _ = 0;\n".repeat(200)
    );
    fs::write(fixture.root.join("z_decl.rs"), declaration).expect("named declaration");
    for query in [
        "Where is SourceLocation.",
        "Where is SourceLocation",
        "Where is SourceLocation?",
        "Where is SourceLocation!",
        "Where is SourceLocation,",
        "Where is 'SourceLocation'.",
    ] {
        let output = fixture.run(&["search", "--max-results=1", query], "compact");
        assert!(output.status.success(), "punctuated named-symbol search");
        assert!(output.stderr.is_empty(), "no named-symbol diagnostics");
        assert_eq!(
            compact_locations(&output),
            ["z_decl.rs:1"],
            "named declaration under cap"
        );
    }
    for query in [
        "Where is SourceLocation.rs",
        "Where is src/SourceLocation.rs.",
        "Where is module.SourceLocation.",
        "memory provider register plugin search transport",
    ] {
        assert!(
            pbi_rs::named_search_terms(query).is_empty(),
            "paths and generic prose are not names"
        );
    }
    assert_eq!(
        pbi_rs::named_search_terms("Where is SourceLocation::display_relative."),
        ["sourcelocation::display_relative"]
    );
    assert_eq!(pbi_rs::named_search_terms("Where is Café."), ["café"]);
    assert_eq!(
        pbi_rs::named_search_terms("Where is \"SourceLocation\"."),
        ["sourcelocation"]
    );
}

#[test]
fn generic_plugin_bag_keeps_late_executable_anchor_before_declaration_noise() {
    let fixture = Fixture::new();
    let query = "Orbit Vector memory provider register plugin search transport Cache RPC config integration reserved stub";
    let plugin = "contrib/orbit-agent-plugin/vector/__init__.py";
    fs::create_dir_all(fixture.root.join("tests")).expect("noise directory");
    fs::create_dir_all(fixture.root.join("docs")).expect("documentation directory");
    fs::create_dir_all(fixture.root.join(plugin).parent().expect("plugin parent"))
        .expect("plugin directory");
    for (index, word) in [
        "memory",
        "provider",
        "register",
        "search",
        "transport",
        "config",
        "integration",
        "stub",
    ]
    .iter()
    .enumerate()
    {
        fs::write(
            fixture.root.join(format!("tests/noise_{index}.rs")),
            format!("pub fn {word}() {{}}\n"),
        )
        .expect("generic declaration noise");
    }
    fs::write(
        fixture.root.join("docs/noise.md"),
        "Orbit Vector Cache RPC config integration reserved stub\n",
    )
    .expect("documentation noise");
    fs::write(fixture.root.join("transport.py"), "pass\n").expect("filename distractor");
    let padding = format!(
        "{}def load_config():\n    return \"{}\"\n{}",
        "unrelated_value = 0\n".repeat(110),
        query.repeat(3),
        "unrelated_value = 0\n".repeat(48)
    );
    let source = format!(
        "\"\"\"{query}.\"\"\"\n{}class VectorMemoryProvider:\n    def search(self, query):\n        return self.transport.search(query)\n\ndef register_plugin(registry):\n    registry.register_memory_provider(VectorMemoryProvider())\n",
        padding
    );
    fs::write(fixture.root.join(plugin), source).expect("late plugin implementation");
    let output = fixture.run(&["search", query], "compact");
    assert!(output.status.success());
    let locations = compact_locations(&output);
    assert!(
        locations
            .iter()
            .any(|location| location.starts_with(&format!("{plugin}:"))),
        "plugin must be admitted inside the default result cap: {locations:?}"
    );
    assert!(
        locations.contains(&format!("{plugin}:162-167")),
        "plugin must cite the late implementation, not its keyword header: {locations:?}"
    );
}

#[test]
fn compact_search_ignores_foreign_literal_declarations() {
    let cases = [
        (
            "decoy.py",
            concat!(
                "DOC = \"\"\"Example plugin source ranker \"first\" second \"third\n",
                "def plugin_source_ranker(): # plugin source ranker\n",
                "    pass\n",
                "\"\"\"\n"
            ),
            "def plugin_source_ranker(): # plugin source ranker\n    pass\n",
        ),
        (
            "decoy.js",
            concat!(
                "const docs = `Example plugin source ranker:\n",
                "function plugin_source_ranker() { // plugin source ranker\n",
                "  return undefined;\n",
                "}\n",
                "`;\n"
            ),
            "function plugin_source_ranker() { // plugin source ranker\n  return undefined;\n}\n",
        ),
    ];

    for (filename, literal, implementation) in cases {
        let fixture = Fixture::new();
        let implementation_line = literal.lines().count() + 51;
        let source = format!("{literal}{}{implementation}", "\n".repeat(50));
        fs::write(fixture.root.join(filename), source).expect("foreign literal fixture");
        let output = fixture.run(
            &["search", "--max-results=1", "plugin source ranker"],
            "compact",
        );
        assert!(output.status.success(), "compact search must succeed");
        let locations = compact_locations(&output);
        assert!(
            locations
                .iter()
                .any(|location| location.starts_with(&format!("{filename}:{implementation_line}"))),
            "executable implementation must outrank its literal example: {locations:?}"
        );
        assert!(
            !locations
                .iter()
                .any(|location| location.starts_with(&format!("{filename}:1"))),
            "literal declaration must not be selected: {locations:?}"
        );
    }
}

#[test]
fn typescript_template_literal_types_exclude_pseudo_declarations() {
    for literal in [
        "`prefix ${string}\r\nfunction hidden_ranker() {}\r\n`",
        "`prefix\r\nfunction hidden_ranker() {}\r\n`",
        "`prefix ${`inner ${string}\r\nfunction nested_ranker() {}\r\n`}\r\nfunction hidden_ranker() {}\r\n`",
    ] {
        let fixture = Fixture::new();
        let source = format!("type Label = {literal};\r\n{}function plugin_source_ranker() {{ return 1; }} // plugin source ranker\r\n", "\r\n".repeat(50));
        let line = source.lines().count();
        fs::write(fixture.root.join("owner.ts"), source).expect("template type fixture");
        let ranked = fixture.run(&["search", "--max-results=1", "plugin source ranker"], "template-type");
        assert!(ranked.status.success(), "literal={literal}");
        assert_eq!(compact_locations(&ranked), [format!("owner.ts:{line}")]);
        let positive = fixture.run(&["How does plugin_source_ranker work?"], "template-type");
        assert!(positive.status.success(), "literal={literal}");
        assert_eq!(String::from_utf8_lossy(&positive.stdout), format!("owner.ts:{line}\n"));
        for query in ["How does hidden_ranker work?", "How does nested_ranker work?"] {
            let negative = fixture.run(&[query], "template-type");
            assert_eq!(negative.status.code(), Some(1), "literal={literal}, query={query}");
            assert!(negative.stdout.is_empty(), "literal={literal}, query={query}");
        }
    }
}

#[test]
fn compact_search_ignores_foreign_regex_literal_declarations() {
    let fixture = Fixture::new();
    let literal =
        r#"const matcher = /["'] café function plugin_source_ranker plugin source ranker/;"#;
    let implementation = "function plugin_source_ranker() { return 0; }";
    let source = format!("{literal}\r\n{}{implementation}\r\n", "\r\n".repeat(50));
    fs::write(fixture.root.join("owner.js"), source).expect("regex literal fixture");
    let output = fixture.run(
        &["search", "--max-results=1", "plugin source ranker"],
        "compact",
    );
    assert!(output.status.success(), "compact search must succeed");
    assert_eq!(
        compact_locations(&output),
        ["owner.js:52"],
        "the regex declaration-like literal is negative and following executable source is positive"
    );
}

#[test]
fn compact_search_javascript_operand_division_preserves_executable_source() {
    let mut failures = Vec::new();
    for operand in [
        "obj.return",
        "obj?.throw",
        "obj. /* gap */ new",
        "obj. // gap\r\nreturn",
        "of",
        "await",
        "yield",
        "obj.value",
        "obj.value!",
        "obj.return!",
        "(obj.value)!",
        "éreturn",
        "caféreturn",
        "obj.caféreturn",
        "éthrow",
        "évalue",
        "obj.cafévalue",
        "this.#return",
        "this?. /* gap */ #throw",
        "this.#value",
        r"\u{e9}return",
        r"obj.caf\u{e9}return",
    ] {
        let fixture = Fixture::new();
        let source = if operand.starts_with("this") {
            format!("class C {{\r\n #return = 1; #throw = 1; #value = 1;\r\n m() {{ const value = {operand} / function plugin_source_ranker() {{ return 1; }} / 2; }}\r\n}}\r\n")
        } else {
            format!("const obj = {{ return: 1, throw: 1, new: 1, value: 1 }}; const of = 1, await = 1, yield = 1, éreturn = 1, caféreturn = 1, éthrow = 1, évalue = 1;\r\nconst value = {operand} / function plugin_source_ranker() {{ return 1; }} / 2;\r\n")
        };
        let line = source
            .lines()
            .position(|line| line.contains("function plugin_source_ranker"))
            .expect("function expression line")
            + 1;
        let filename = if operand.ends_with('!') {
            "owner.ts"
        } else {
            "owner.js"
        };
        fs::write(fixture.root.join(filename), source).expect("division fixture");
        for args in [
            vec!["search", "--max-results=1", "plugin_source_ranker"],
            vec!["How does plugin_source_ranker work?"],
        ] {
            let output = fixture.run(&args, "division");
            let locations = if args[0] == "search" {
                compact_locations(&output)
            } else {
                String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .map(str::to_owned)
                    .collect()
            };
            if !output.status.success() || locations != [format!("{filename}:{line}")] {
                failures.push((operand, args, output.status.code(), locations));
            }
        }
    }
    assert!(failures.is_empty(), "division source lost: {failures:?}");
}

#[test]
fn compact_typescript_type_close_division() {
    let mut failures = Vec::new();
    for source in [
            "const identity = <T>(x: T) => x;\nconst value = identity<number> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const obj = { method: <T>(x: T) => x };\nconst value = obj.method<number> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as Array<number> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as () => number / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = [] satisfies Array<number> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = {} as {x:number} / function plugin_source_ranker() { return 1; } / 2;\n",
            "const identity = <T>(x: T) => x;\nconst value = identity<Array<number>> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const identity = <T>(x: T) => x;\nconst value = (identity<number>) / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = (1 as Array<number>) / function plugin_source_ranker() { return 1; } / 2;\n",
            "const identity = <T>(x: T) => x;\nconst value = identity<number> /*gap*/ / function plugin_source_ranker() { return 1; } / 2;\n",
            "const identity = <T>(x: T) => x;\nconst value = identity<number>\n / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = (1 as Array<number>)! / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as Array<number> / function plugin_source_ranker() { return 1; } / 2;\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\n\nfunction following_owner(){return 2;}\n",
            "const value = 1 as number / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as Array<() => number> / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as [number, number] / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as (number | string) / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as \"value\" / function plugin_source_ranker() { return 1; } / 2;\n",
            "const value = 1 as Array<{x:number; y:number}> / function plugin_source_ranker() { return 1; } / 2;\n",
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("owner.ts"), source).expect("valid TypeScript fixture");
        let line = source.lines().position(|line| line.contains("function plugin_source_ranker")).expect("operand declaration") + 1;
        let output = fixture.run(&["How does plugin_source_ranker work?"], "type-close");
        let actual = String::from_utf8_lossy(&output.stdout);
        if !output.status.success() || actual != format!("owner.ts:{line}\n") {
            failures.push((source, output.status.code(), actual.into_owned()));
        }
    }
    assert!(
        failures.is_empty(),
        "type-close division lost: {failures:?}"
    );
    for prefix in [
        "1 >",
        "1 >=",
        "1 >>",
        "1 >>>",
        "1 <",
        "1 <=",
        "1 <<",
        "1 < 2; const next = 1 >",
    ] {
        let fixture = Fixture::new();
        let source = format!("const value = {prefix} /[\"'] function hidden_ranker/;\n{}function plugin_source_ranker() {{ return 1; }}\n", "\n".repeat(50));
        fs::write(fixture.root.join("owner.ts"), source).expect("comparison regex fixture");
        let positive = fixture.run(&["How does plugin_source_ranker work?"], "comparison");
        assert!(positive.status.success(), "prefix={prefix}");
        assert_eq!(
            String::from_utf8_lossy(&positive.stdout),
            "owner.ts:52\n",
            "prefix={prefix}"
        );
        let negative = fixture.run(&["How does hidden_ranker work?"], "comparison");
        assert_eq!(negative.status.code(), Some(1), "prefix={prefix}");
        assert!(negative.stdout.is_empty(), "prefix={prefix}");
    }
}

#[test]
fn language_owner_rust_adjacent_literals_preserve_behavior() {
    let mut results = Vec::new();
    for literals in [
        "r\"\"\"docs\"",
        "\"\"\"docs\"",
        "r\"\" \"docs\"",
        "\"\" \"docs\"",
    ] {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("source.rs"), format!(
            "macro_rules! adjacent {{ ($a:literal $b:literal) => {{}}; }}\nadjacent!({literals});\npub fn parse_conversion(input: &str) -> Result<u64, std::num::ParseIntError> {{\n    input.parse::<u64>()\n}}\n"
        )).expect("legal adjacent literal fixture");
        let output = fixture.run(
            &["How does parse_conversion parse input and handle conversion errors?"],
            "behavior",
        );
        results.push((
            output.status.code(),
            String::from_utf8_lossy(&output.stdout)
                .lines()
                .map(str::to_owned)
                .collect::<Vec<_>>(),
        ));
    }
    assert_eq!(results, vec![(Some(0), vec!["source.rs:3".to_string()]); 4]);
}

#[test]
fn language_owner_foreign_literals_comments_preserve_real_anchor() {
    let cases = [
        ("py", "# plugin source ranker unmatched \"\"\" /* `\n", "def plugin_source_ranker():\n    return 0\n"),
        ("py", "note = 'plugin source ranker \\\ndef plugin_source_ranker():\\\nend'\n", "def plugin_source_ranker():\n    return 0\n"),
        ("py", "note = r\"\\\"\" # plugin source ranker\n", "def plugin_source_ranker():\n    return 0\n"),
        ("py", "note = f'''plugin source ranker\ndef plugin_source_ranker():\n'''\n", "def plugin_source_ranker():\n    return 0\n"),
        ("js", "/* plugin source ranker /* example */\n", "function plugin_source_ranker() { return 0; }\n"),
        ("ts", "const docs = 'plugin source ranker \\\nfunction plugin_source_ranker() {}\\\nend';\n", "function plugin_source_ranker() { return 0; }\n"),
        ("js", "const docs = `plugin source ranker ${`inner\nfunction plugin_source_ranker() {}\n`} tail`;\n", "function plugin_source_ranker() { return 0; }\n"),
        ("cpp", "const char* docs = R\"tag(plugin source ranker \"\nclass PluginSourceRanker {};\n)tag\";\n", "class PluginSourceRanker {};\n"),
        ("c", "// plugin source ranker \\\nint plugin_source_ranker();\n", "int plugin_source_ranker() { return 0; }\n"),
        ("h", "// plugin source ranker \\\r\nint plugin_source_ranker();\r\n", "int plugin_source_ranker() { return 0; }\n"),
        ("c", "const char* docs = \"\"\"plugin source ranker\";\n/* example /* nested marker */\nint marker = '\"x';\n", "int plugin_source_ranker() { return 0; }\n"),
    ];
    let mut results = Vec::new();
    let mut expected = Vec::new();
    for (extension, prefix, implementation) in cases {
        let fixture = Fixture::new();
        let filename = format!("owner.{extension}");
        let line = prefix.lines().count() + 51;
        fs::write(
            fixture.root.join(&filename),
            format!("{prefix}{}{implementation}", "\n".repeat(50)),
        )
        .expect("owned lexical fixture");
        let output = fixture.run(
            &["search", "--max-results=1", "plugin source ranker"],
            "compact",
        );
        results.push((output.status.code(), compact_locations(&output)));
        expected.push((Some(0), vec![format!("{filename}:{line}")]));
    }
    assert_eq!(results, expected);
}

#[test]
fn foreign_component_hints_do_not_expand_raw_admission() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("owner.py"),
        "def plugin_source_ranker():\n    return 0\n",
    )
    .expect("component-only source");
    for (options, query) in [
        (vec![], "plugin source ranker"),
        (vec!["--bm25"], "plugin source ranker"),
        (vec!["--bm25", "--exact"], "plugin source ranker"),
        (
            vec!["--bm25", "--strict-elastic-syntax"],
            "plugin AND source AND ranker",
        ),
        (vec![], "plugin OR source OR ranker"),
    ] {
        let mut args = vec!["search"];
        args.extend(options);
        args.push(query);
        let output = fixture.run(&args, "component-only");
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn bounded_regex_returns_original_source_lines_without_snippets() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("类型.rs"),
        "// synthetic\nfn café() {}\nfn café() {}\n",
    )
    .expect("synthetic Unicode source");
    let output = fixture.run(
        &["search", "--regex", r"^fn café\(", "--timeout=2"],
        "regex",
    );
    assert!(output.status.success(), "regex search must succeed");
    assert_eq!(compact_locations(&output), ["类型.rs:2", "类型.rs:3"]);
    assert!(output.stderr.is_empty());
    assert!(!String::from_utf8_lossy(&output.stdout).contains("fn café"));
}

#[test]
fn bounded_regex_private_usage_errors_and_flag_contract() {
    let fixture = Fixture::new();
    let oversized = "é".repeat(4097);
    let deep = format!("{}a{}", "(".repeat(65), ")".repeat(65));
    for pattern in [
        "[synthetic_private\n\r\t\u{1b}",
        r"(?=synthetic_private)",
        r"[a-z]{1000000}",
        &oversized,
        &deep,
    ] {
        let output = fixture.run(&["search", "--regex", pattern], "regex");
        assert_eq!(
            output.status.code(),
            Some(2),
            "invalid regex must be usage error"
        );
        assert!(output.stdout.is_empty());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            !stderr.contains(pattern),
            "rejected pattern must remain private"
        );
        assert!(!stderr.contains("synthetic_private"));
        assert!(!stderr.chars().any(|ch| ch.is_control() && ch != '\n'));
    }
    for flag in [
        "--exact",
        "--stem",
        "--strict-elastic-syntax",
        "--session=synthetic",
        "--bm25",
        "--files-only",
        "--frequency",
        "--exclude-filenames",
        "--regex",
    ] {
        let output = fixture.run(&["search", "--regex", flag, "synthetic"], "regex");
        assert_eq!(
            output.status.code(),
            Some(2),
            "incompatible flag must be refused"
        );
        assert!(output.stdout.is_empty());
    }
}

#[test]
fn bounded_regex_no_hit_and_nested_quantifiers_finish_under_deadline() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("adversarial.txt"),
        format!("{}!\n", "a".repeat(128 * 1024)),
    )
    .expect("synthetic adversarial source");
    let started = Instant::now();
    let output = fixture.run(&["search", "--regex", "^(a+)+$", "--timeout=2"], "regex");
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "bounded engine must finish under deadline"
    );
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    assert!(String::from_utf8_lossy(&output.stderr).starts_with("pbi: no source locations found\n"));
    let zero = fixture.run(&["search", "--regex", "a", "--timeout=0"], "regex");
    assert_eq!(zero.status.code(), Some(1));
    assert!(zero.stdout.is_empty());
    assert_eq!(
        failure_fields(std::str::from_utf8(&zero.stderr).expect("static failure"))["stage_status"],
        "deadline"
    );
}

#[test]
fn bounded_regex_preserves_admission_and_actual_utf8_lines() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("synthetic source directory");
    for name in [
        "src/kept.rs",
        "src/ignored.rs",
        "src/policy.rs",
        ".env",
        ".hidden.rs",
        "src/control\n.rs",
    ] {
        fs::write(fixture.root.join(name), "// synthetic\nfn café() {}\n")
            .expect("synthetic source");
    }
    fs::write(fixture.root.join(".gitignore"), "src/policy.rs\n!.*\n")
        .expect("synthetic ignore policy");
    fs::write(fixture.base.join("outside.rs"), "fn café() {}\n").expect("synthetic outside");
    symlink(
        fixture.base.join("outside.rs"),
        fixture.root.join("linked.rs"),
    )
    .expect("source symlink");
    symlink("/proc", fixture.root.join("foreign")).expect("cross-device link");
    fs::write(
        fixture.root.join("src/large.rs"),
        "a".repeat(2 * 1024 * 1024 + 1),
    )
    .expect("oversized source");
    let output = fixture.run_args(
        "regex",
        &[
            "search",
            "--regex",
            r"^fn café\(",
            "--language=rust",
            "--ignore=src/ignored.rs",
        ],
    );
    assert!(output.status.success(), "admitted regex match must succeed");
    assert_eq!(compact_locations(&output), ["src/kept.rs:2"]);
    let missed = fixture.run_args("regex", &["search", "--regex", "linked|kept|large"]);
    assert_eq!(
        missed.status.code(),
        Some(1),
        "filename-only regex must not invent locations"
    );
}

#[test]
fn bounded_regex_candidate_root_result_and_output_caps() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("many.txt"), "synthetic\n".repeat(4097))
        .expect("candidate overflow");
    let overflow = fixture.run(&["search", "--regex", "synthetic"], "regex");
    assert_eq!(overflow.status.code(), Some(1));
    assert!(overflow.stdout.is_empty());
    assert_eq!(
        String::from_utf8_lossy(&overflow.stderr).lines().next(),
        Some("pbi-rs: native search exceeded its bounded limit")
    );
    fs::write(fixture.root.join("many.txt"), "synthetic\n".repeat(4)).expect("small candidate set");
    let capped = fixture.run(
        &["search", "--regex", "synthetic", "--max-results=2"],
        "regex",
    );
    assert!(capped.status.success());
    assert_eq!(compact_locations(&capped), ["many.txt:1", "many.txt:2"]);
    for index in 0..17 {
        fs::write(
            fixture.root.join(format!("root-{index}.txt")),
            "synthetic\n",
        )
        .expect("root traversal fixture");
    }
    let root_search = fixture.run(
        &["search", "--regex", "synthetic", "--max-results=32"],
        "regex",
    );
    assert!(
        root_search.status.success(),
        "a selected root with more than 16 children must be searched"
    );
    let mut expected = Vec::new();
    let mut root_children = 0;
    for entry in fs::read_dir(&fixture.root).expect("enumerate fixture root") {
        let entry = entry.expect("fixture root entry");
        root_children += 1;
        if !entry.file_type().expect("fixture entry type").is_file() {
            continue;
        }
        let name = entry
            .file_name()
            .into_string()
            .expect("fixture filename is UTF-8");
        for (index, line) in fs::read_to_string(entry.path())
            .expect("read expected fixture source")
            .lines()
            .enumerate()
        {
            if line.contains("synthetic") {
                expected.push(format!("{name}:{}", index + 1));
            }
        }
    }
    assert!(root_children > 16, "fixture must exceed the old root cap");
    expected.sort();
    let mut locations = compact_locations(&root_search);
    locations.sort();
    assert_eq!(expected.len(), 21, "fixture's admitted match set");
    assert_eq!(
        locations, expected,
        "search must return the exact admitted set"
    );

    let fixture = Fixture::new();
    let name = format!("{}.txt", "n".repeat(240));
    fs::write(fixture.root.join(name), "synthetic\n".repeat(4096)).expect("output overflow");
    let bounded = fixture.run(
        &["search", "--regex", "synthetic", "--max-results=4096"],
        "regex",
    );
    assert!(
        bounded.status.success(),
        "output truncation must retain nonempty bounded result"
    );
    assert!(!bounded.stdout.is_empty() && bounded.stdout.len() <= 64 * 1024);
    assert!(String::from_utf8_lossy(&bounded.stderr).contains("truncated"));
}

const SCOPE_QUERY: &str = "compression publication cache assembly";

fn compact_locations(output: &Output) -> Vec<String> {
    let stdout = String::from_utf8(output.stdout.clone()).expect("UTF-8 compact output");
    let lines = stdout.lines().collect::<Vec<_>>();
    assert_eq!(lines.len() % 2, 0, "paired locations and scores");
    lines
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            assert!(pair[1]
                .strip_prefix("Score: ")
                .and_then(|score| score.parse::<f64>().ok())
                .is_some());
            pair[0].to_owned()
        })
        .collect()
}

#[test]
fn unified_search_bm25_numeric_ranking() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("fixture.rs")).expect("remove seed");
    for (name, text) in [
        ("high.txt", "foo foo foo foo\n"),
        ("one.txt", "foo\n"),
        (
            "long.txt",
            "foo noise noise noise noise noise noise noise noise noise\n",
        ),
    ] {
        fs::write(fixture.root.join(name), text).expect("rank fixture");
    }
    let default = fixture.run(&["search", "foo"], "default");
    let bm25 = fixture.run(&["search", "--bm25", "foo"], "bm25");
    assert!(default.status.success());
    assert!(bm25.status.success());
    let scores = |output: &Output| {
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter_map(|line| {
                line.strip_prefix("Score: ")
                    .map(|value| value.parse::<f64>().expect("numeric score"))
            })
            .collect::<Vec<_>>()
    };
    let actual = scores(&default);
    assert_eq!(actual.len(), 3, "default must print BM25 scores");
    assert_eq!(actual, scores(&bm25));
    // Filename tokens are included by the existing ranker: lengths 6, 3, 12.
    let average = 7.0;
    let idf = (1.0_f64 + 0.5 / 3.5).ln();
    let mut expected = [(4.0, 6.0), (1.0, 3.0), (1.0, 12.0)]
        .map(|(tf, length)| idf * tf * 2.2 / (tf + 1.2 * (0.25 + 0.75 * length / average)));
    expected.sort_by(|left, right| right.total_cmp(left));
    for (actual, expected) in actual.into_iter().zip(expected) {
        assert!((actual - expected).abs() < 0.00005);
    }
}

#[test]
fn unified_search_boolean_phrase_and_unicode_sets() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("fixture.rs")).expect("remove seed");
    for (name, text) in [
        ("a.txt", "foo bar café 类型\n"),
        ("b.txt", "bar foo 类型 café\n"),
        ("c.txt", "foo\n"),
        ("d.txt", "bar\n"),
    ] {
        fs::write(fixture.root.join(name), text).expect("query fixture");
    }
    for (query, expected) in [
        ("foo OR bar", vec!["a.txt", "b.txt", "c.txt", "d.txt"]),
        ("foo AND bar", vec!["a.txt", "b.txt"]),
        ("foo NOT bar", vec!["c.txt"]),
        ("\"foo bar\"", vec!["a.txt"]),
        ("\"café 类型\"", vec!["a.txt"]),
    ] {
        for raw in [false, true] {
            let args = if raw {
                vec!["search", "--bm25", query]
            } else {
                vec!["search", query]
            };
            let output = fixture.run(&args, "unified");
            assert!(
                output.status.success(),
                "{query}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let stdout = String::from_utf8_lossy(&output.stdout);
            let actual = ["a.txt", "b.txt", "c.txt", "d.txt"]
                .into_iter()
                .filter(|name| stdout.contains(name))
                .collect::<Vec<_>>();
            assert_eq!(actual, expected, "{query}, raw={raw}");
        }
    }
}

#[test]
fn english_stemming_is_opt_in_and_preserves_queries() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("fixture.rs")).expect("remove seed");
    for (name, text) in [
        ("a.rs", "run alpha\n"),
        ("b.rs", "walk\n"),
        ("c.rs", "retries retrying\n"),
        ("d.rs", "if or in type\n"),
    ] {
        fs::write(fixture.root.join(name), text).expect("stem fixture");
    }
    let search = |query: &str, stem: bool, raw: bool| {
        let mut args = vec!["search"];
        if stem {
            args.push("--stem");
        }
        if raw {
            args.push("--bm25");
        }
        args.push(query);
        fixture.run(&args, "stemming")
    };

    for raw in [false, true] {
        let stemmed = search("running", true, raw);
        assert!(stemmed.status.success(), "stem search must succeed");
        let output = String::from_utf8_lossy(&stemmed.stdout);
        assert!(output.contains("a.rs"), "stem hit missing");
        assert!(!output.contains("b.rs"), "unrelated hit");

        let default = search("running", false, raw);
        assert_eq!(default.status.code(), Some(1));
        assert!(default.stdout.is_empty());

        let retry = search("retry", true, raw);
        assert!(retry.status.success());
        assert!(String::from_utf8_lossy(&retry.stdout).contains("c.rs"));

        for stopword in ["if", "or", "in", "type"] {
            let result = search(stopword, true, raw);
            assert!(result.status.success(), "stopword search failed");
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("d.rs"),
                "stopword hit missing"
            );
        }

        for query in ["running AND alpha", "\"running alpha\""] {
            let result = search(query, true, raw);
            assert!(
                result.status.success(),
                "Boolean or phrase stem search failed"
            );
            assert!(
                String::from_utf8_lossy(&result.stdout).contains("a.rs"),
                "expression hit missing"
            );
        }
        let reversed = search("\"alpha running\"", true, raw);
        assert_eq!(reversed.status.code(), Some(1));
        assert!(reversed.stdout.is_empty());
    }
}

#[test]
fn english_stemming_bounds_exact_refusal_and_session_separation() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("a.txt"),
        "run retries café 类型 retry_id\n",
    )
    .expect("synthetic source");
    for args in [
        vec!["search", "--stem", "--bm25", "--exact", "[REDACTED]"],
        vec!["search", "--exact", "--stem", "[REDACTED]"],
    ] {
        let result = fixture.run_args("stem", &args);
        assert_eq!(result.status.code(), Some(2));
        assert!(result.stdout.is_empty());
        assert!(String::from_utf8_lossy(&result.stderr)
            .contains("--stem cannot be combined with --exact"));
    }
    let long = "a".repeat(8193);
    let result = fixture.run_args("stem", &["search", "--stem", &long]);
    assert_eq!(result.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&result.stderr).contains(&long));
    for query in [
        "café",
        "类型",
        "retry_id",
        "running NOT walk",
        "running OR walk",
    ] {
        assert!(fixture
            .run_args("stem", &["search", "--stem", "--bm25", query])
            .status
            .success());
    }
    for query in ["running NOT retries", "\"retry id\""] {
        assert_eq!(
            fixture
                .run_args("stem", &["search", "--stem", "--bm25", query])
                .status
                .code(),
            Some(1)
        );
    }
    let plain = ["search", "--bm25", "--session=stem-scope", "run"];
    assert!(fixture.run_session(&plain).status.success());
    assert_eq!(fixture.run_session(&plain).status.code(), Some(1));
    assert!(fixture
        .run_session(&["search", "--bm25", "--stem", "--session=stem-scope", "run"])
        .status
        .success());
}

#[test]
fn unified_search_punctuated_operands_preserve_default_raw_parity() {
    let fixture = Fixture::new();
    fs::remove_file(fixture.root.join("fixture.rs")).expect("remove seed");
    for (name, text) in [
        ("a.txt", "Owner:OR Owner:AND Owner:NOT Owner::OR\n"),
        ("b.txt", "OR AND NOT\n"),
        ("c.txt", "Owner\n"),
        ("path.txt", "src/Owner:OR\n"),
    ] {
        fs::write(fixture.root.join(name), text).expect("punctuation fixture");
    }
    let mut statuses = Vec::new();
    for (query, expected) in [
        ("Owner:OR", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("Owner:AND", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("Owner:NOT", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("Owner::OR", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("Owner.OR", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("Owner-OR", vec!["a.txt", "b.txt", "c.txt", "path.txt"]),
        ("\"Owner:OR\"", vec!["a.txt", "path.txt"]),
        ("\"Owner:AND\"", vec!["a.txt"]),
        ("\"Owner:NOT\"", vec!["a.txt"]),
        ("\"Owner::OR\"", vec!["a.txt"]),
        ("\"src/Owner:OR\"", vec!["path.txt"]),
        ("\"Owner:OR\" OR \"Owner:AND\"", vec!["a.txt", "path.txt"]),
        ("(\"Owner:OR\" OR \"Owner:AND\") NOT src", vec!["a.txt"]),
    ] {
        let default = fixture.run(&["search", query], "default");
        let raw = fixture.run(&["search", "--bm25", "--format=json", query], "raw");
        statuses.extend([default.status.code(), raw.status.code()]);
        if !default.status.success() || !raw.status.success() {
            continue;
        }
        assert!(default.stderr.is_empty() && raw.stderr.is_empty());
        let rows: serde_json::Value = serde_json::from_slice(&raw.stdout).expect("raw JSON");
        let mut raw_files = rows
            .as_array()
            .expect("rows")
            .iter()
            .map(|row| row["file"].as_str().expect("file").to_owned())
            .collect::<Vec<_>>();
        let mut default_files = compact_locations(&default)
            .into_iter()
            .map(|location| location.split_once(':').expect("location").0.to_owned())
            .collect::<Vec<_>>();
        raw_files.sort();
        default_files.sort();
        assert_eq!(raw_files, expected, "raw punctuation file set");
        assert_eq!(default_files, expected, "default punctuation file set");
    }
    assert!(
        statuses.iter().all(|status| *status == Some(0)),
        "punctuated operands must not manufacture operators: {statuses:?}"
    );
}

#[test]
fn unified_search_phrase_rejects_reversed_tokens() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("ordered.txt"), "foo bar\n").expect("ordered");
    fs::write(fixture.root.join("reversed.txt"), "bar foo\n").expect("reversed");
    let output = fixture.run(&["search", "\"foo bar\""], "phrase");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ordered.txt"));
    assert!(!stdout.contains("reversed.txt"), "{stdout}");
}

#[test]
fn unified_search_malformed_syntax_is_private() {
    let fixture = Fixture::new();
    for query in ["foo OR", "\"private_marker", "foo AND (bar OR)", "NOT foo"] {
        for raw in [false, true] {
            let args = if raw {
                vec!["search", "--bm25", query]
            } else {
                vec!["search", query]
            };
            let output = fixture.run(&args, "malformed");
            assert_eq!(output.status.code(), Some(2), "{query}, raw={raw}");
            assert!(output.stdout.is_empty());
            assert!(!String::from_utf8_lossy(&output.stderr).contains("private_marker"));
        }
    }
}

#[test]
fn search_c_method_name_returns_bm25_source_blocks() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("registration.c"),
        "/* RestartUnit is mentioned only in this comment. */\n\
         static const BusVTable methods[] = {\n\
             SD_BUS_METHOD_WITH_ARGS(\"RestartUnit\",\n\
                 SD_BUS_ARGS(\"s\", name), handler, 0),\n\
         };\n",
    )
    .expect("C registration");
    fs::write(
        fixture.root.join("notes.md"),
        "RestartUnit reference only\n",
    )
    .expect("unrelated documentation");

    let output = fixture.run(&["search", "RestartUnit"], "verified");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        compact_locations(&output),
        ["notes.md:1", "registration.c:1-3"]
    );
}

#[test]
fn search_rust_type_name_prioritizes_declaration_block() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("declaration.rs"),
        "// SourceLocation occurs in this comment.\npub struct SourceLocation;\n",
    )
    .expect("Rust declaration");
    fs::write(
        fixture.root.join("notes.md"),
        "SourceLocation reference only\n",
    )
    .expect("unrelated documentation");

    let output = fixture.run(&["search", "SourceLocation"], "verified");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        compact_locations(&output),
        ["declaration.rs:1-2", "notes.md:1"]
    );
}

#[test]
fn field_question_admits_the_type_declaration_before_repeated_mentions() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("model.rs"),
        "pub struct LedgerState {\n    pub entries: usize,\n    pub revision: u64,\n}\n",
    )
    .expect("type declaration");
    fs::write(
        fixture.root.join("usage.rs"),
        "// LedgerState fields store cached data.\n".repeat(40),
    )
    .expect("distracting mentions");
    let output = fixture.run(&["What fields does LedgerState store?"], "evidence");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("model.rs:1"), "{stdout}");
}

#[test]
fn raw_symbol_definitions_outrank_fixture_strings_without_excluding_tests() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.root.join("src")).expect("source directory");
    fs::create_dir_all(fixture.root.join("tests")).expect("test directory");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "pub struct SourceLocation {\n    path: PathBuf,\n}\n\nimpl SourceLocation {\n    pub fn display_relative(&self, root: &Path) -> String {\n        root.display().to_string()\n    }\n}\n",
    )
    .expect("production declarations");
    let mut tests = String::from(
        "#[test]\nfn positional_question_dispatches_through_adk_and_checks_citations() {\n",
    );
    for _ in 0..12 {
        tests.push_str("    fs::write(path, \"pub struct SourceLocation; fn display_relative() {}\").unwrap();\n".repeat(4).as_str());
        tests.push_str(&"    unrelated_setup();\n".repeat(6));
    }
    tests.push_str("}\nfn bounded_output_with_stdout() {}\n");
    fs::write(fixture.root.join("tests/contracts.rs"), tests).expect("fixture definitions");

    let output = fixture.run(
        &["search", "--bm25", "SourceLocation display_relative"],
        "raw",
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "production declaration search failed"
    );
    assert!(
        stdout.contains("File: src/lib.rs, Lines: 1-6\n"),
        "production declaration missing from results"
    );
    for query in [
        "positional_question_dispatches_through_adk_and_checks_citations",
        "bounded_output_with_stdout",
    ] {
        let output = fixture.run(&["search", "--bm25", query], "raw");
        assert!(
            output.status.success(),
            "explicit test-symbol raw search failed"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("File: tests/contracts.rs"));
        let output = fixture.run(&["search", query], "verified");
        assert!(
            output.status.success(),
            "explicit test-symbol verified search failed"
        );
        assert!(String::from_utf8_lossy(&output.stdout).contains("tests/contracts.rs:"));
    }
}

#[test]
fn raw_unicode_declarations_keep_priority_under_result_cap() {
    for name in ["Éclair", "éclair", "Δέλτα", "类型"] {
        let fixture = Fixture::new();
        fs::write(fixture.root.join("a_noise.rs"), "pub fn helper() {}\n")
            .expect("competing ASCII declaration");
        fs::write(
            fixture.root.join("z_decl.rs"),
            format!(
                "pub struct {name};\n// {}\n",
                format!("{name} helper ").repeat(40)
            ),
        )
        .expect("higher-scoring Unicode declaration");
        let query = format!("{name} helper");
        let strict_query = format!("\"{name}\" OR helper");
        for strict in [false, true] {
            let mut args = vec!["search", "--bm25", "--max-results", "1"];
            if strict {
                args.push("--strict-elastic-syntax");
            }
            args.push(if strict { &strict_query } else { &query });
            let output = fixture.run(&args, "raw");
            let stdout = String::from_utf8_lossy(&output.stdout);
            assert!(output.status.success(), "Unicode CLI search failed");
            assert!(
                stdout.starts_with("File: z_decl.rs, Lines: 1-2\n"),
                "Unicode declaration must be first under cap"
            );
            assert!(
                stdout.contains(&format!("pub struct {name};")),
                "Unicode declaration identity"
            );
        }
    }
    for name in ["Éclair", "Δέλτα", "类型", "İclair"] {
        let source = format!("pub struct {name};\n");
        assert_eq!(
            pbi_rs::matching_rust_declaration_lines(&source, &[name.to_lowercase()]),
            vec![1],
            "normalized Unicode declaration identity"
        );
        for term in ["i", "écl", "clair", "δέλ", "类", "éclair_extra"] {
            assert!(pbi_rs::matching_rust_declaration_lines(&source, &[term.into()]).is_empty());
        }
    }
}

#[test]
fn raw_native_bm25_ranks_source_and_returns_real_locations() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("frequent.rs"),
        "fn needle() { needle(); needle(); }\n",
    )
    .expect("frequent source");
    fs::write(fixture.root.join("rare.rs"), "fn needle() {}\n").expect("rare source");

    let output = fixture.run(&["search", "--bm25", "needle"], "raw");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let text = String::from_utf8_lossy(&output.stdout);
    assert!(
        text.starts_with("File: frequent.rs, Lines: 1-1\n"),
        "{text}"
    );
    assert!(
        text.contains("fn needle() { needle(); needle(); }"),
        "{text}"
    );
    assert!(text.contains("File: rare.rs, Lines: 1-1\n"), "{text}");
}

#[test]
fn raw_blocks_merge_only_within_line_threshold_before_result_cap() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("blocks.rs"),
        "fn marker() {}\n// gap\nfn marker() {}\n// distant 1\n// distant 2\n// distant 3\nfn marker() {}\n",
    )
    .expect("separated matches");
    let separate = fixture.run(
        &[
            "search",
            "--bm25",
            "--format=json",
            "--merge-threshold=0",
            "marker",
        ],
        "raw",
    );
    assert!(
        separate.status.success(),
        "{}",
        String::from_utf8_lossy(&separate.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&separate.stdout).expect("JSON");
    assert_eq!(rows.as_array().expect("rows").len(), 3);
    assert_eq!(rows[0]["line"], 1);
    assert_eq!(rows[1]["line"], 3);
    assert_eq!(rows[2]["line"], 7);

    let merged = fixture.run(
        &[
            "search",
            "--bm25",
            "--format=json",
            "--merge-threshold=1",
            "--max-results=1",
            "marker",
        ],
        "raw",
    );
    assert!(
        merged.status.success(),
        "{}",
        String::from_utf8_lossy(&merged.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&merged.stdout).expect("JSON");
    assert_eq!(rows.as_array().expect("rows").len(), 1);
    assert_eq!(rows[0]["line"], 1);
    assert_eq!(rows[0]["end_line"], 3);
    let files = fixture.run(&["search", "--bm25", "--files-only", "marker"], "raw");
    assert_eq!(files.stdout, b"blocks.rs\n");
}

#[test]
fn raw_strict_elastic_syntax_validates_and_applies_boolean_query() {
    let fixture = Fixture::new();
    fs::write(fixture.root.join("alpha.rs"), "fn alpha() {}\n").expect("alpha");
    fs::write(fixture.root.join("both.rs"), "fn alpha() { beta(); }\n").expect("both");
    fs::write(fixture.root.join("beta.rs"), "fn beta() {}\n").expect("beta");
    fs::write(fixture.root.join("quoted.rs"), "fn camelCase() {}\n").expect("quoted");
    for query in ["alpha beta", "camelCase", "snake_case"] {
        let output = fixture.run(
            &["search", "--bm25", "--strict-elastic-syntax", query],
            "raw",
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "strict query syntax must reject invalid inputs"
        );
    }
    let filtered = fixture.run(
        &[
            "search",
            "--bm25",
            "--strict-elastic-syntax",
            "--format=json",
            "alpha AND NOT beta",
        ],
        "raw",
    );
    assert!(
        filtered.status.success(),
        "{}",
        String::from_utf8_lossy(&filtered.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&filtered.stdout).expect("strict JSON");
    assert_eq!(rows.as_array().expect("rows").len(), 1);
    assert_eq!(rows[0]["file"], "alpha.rs");
    let quoted = fixture.run(
        &[
            "search",
            "--bm25",
            "--strict-elastic-syntax",
            "--format=json",
            "\"camelCase\"",
        ],
        "raw",
    );
    assert!(
        quoted.status.success(),
        "{}",
        String::from_utf8_lossy(&quoted.stderr)
    );
    let rows: serde_json::Value = serde_json::from_slice(&quoted.stdout).expect("quoted JSON");
    assert_eq!(rows[0]["file"], "quoted.rs");
    let grouped = fixture.run(
        &[
            "search",
            "--bm25",
            "--strict-elastic-syntax",
            "--files-only",
            "(alpha OR beta) AND NOT missing",
        ],
        "raw",
    );
    assert!(
        grouped.status.success(),
        "{}",
        String::from_utf8_lossy(&grouped.stderr)
    );
    let paths = String::from_utf8_lossy(&grouped.stdout);
    assert!(
        paths.contains("alpha.rs") && paths.contains("both.rs") && paths.contains("beta.rs"),
        "{paths}"
    );
}

#[test]
fn strict_syntax_failure_redacts_rejected_query_terms() {
    let fixture = Fixture::new();
    for query in ["synthetic_query_token", "SyntheticQueryToken"] {
        let output = fixture.run(
            &["search", "--bm25", "--strict-elastic-syntax", query],
            "raw",
        );
        assert_eq!(
            output.status.code(),
            Some(2),
            "invalid strict syntax remains a usage error"
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.starts_with(
                "pbi-rs: strict query requires quotes around terms with underscores or mixed case\n"
            ),
            "invalid unquoted terms keep the strict-query diagnostic class"
        );
        assert!(
            stderr.contains("argv=search,--bm25,--strict-elastic-syntax,[REDACTED]"),
            "failure receipt must continue redacting query arguments"
        );
        assert!(
            !String::from_utf8_lossy(&output.stdout).contains(query),
            "strict-query failure echoed rejected input to stdout"
        );
        assert!(
            !stderr.contains(query),
            "strict-query failure echoed rejected input to stderr"
        );
    }
}

#[test]
fn raw_native_bm25_json_is_bounded_and_scope_safe() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("kept.rs"), "fn raw_marker() {}\n").expect("kept source");
    fs::write(fixture.root.join("ignored.rs"), "fn raw_marker() {}\n").expect("ignored source");
    fs::write(fixture.base.join("outside.rs"), "fn raw_marker() {}\n").expect("outside source");
    symlink(
        fixture.base.join("outside.rs"),
        fixture.root.join("linked.rs"),
    )
    .expect("linked source");

    let output = fixture.run_args(
        "raw",
        &[
            "search",
            "--bm25",
            "--format=json",
            "--max-results=1",
            "--ignore=ignored.rs",
            "raw_marker",
        ],
    );
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let hits: serde_json::Value = serde_json::from_slice(&output.stdout).expect("valid JSON");
    let rows = hits.as_array().expect("array of hits");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["file"], "kept.rs");
    assert_eq!(rows[0]["line"], 1);
    assert!(rows[0]["score"].as_f64().is_some());
    assert!(rows[0]["snippet"]
        .as_str()
        .is_some_and(|line| line.contains("raw_marker")));
    assert!(!String::from_utf8_lossy(&output.stdout).contains("outside.rs"));
}

#[test]
fn raw_native_result_filters_and_output_budget_change_behavior() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("match.rs"),
        "fn budget_marker() { budget_marker(); }\n",
    )
    .expect("source");
    let files = fixture.run(
        &["search", "--bm25", "--files-only", "budget_marker"],
        "raw",
    );
    assert!(
        files.status.success(),
        "{}",
        String::from_utf8_lossy(&files.stderr)
    );
    assert_eq!(files.stdout, b"match.rs\n");

    let exact = fixture.run(
        &[
            "search",
            "--bm25",
            "--exact",
            "budget_marker() {",
            "--max-results=1",
        ],
        "raw",
    );
    assert!(
        exact.status.success(),
        "{}",
        String::from_utf8_lossy(&exact.stderr)
    );
    assert!(String::from_utf8_lossy(&exact.stdout).contains("match.rs"));

    let bounded = fixture.run(
        &["search", "--bm25", "--max-bytes=4", "budget_marker"],
        "raw",
    );
    assert!(!bounded.status.success());
    assert!(bounded.stdout.len() <= 4);
}

#[test]
fn raw_filename_match_does_not_invent_a_source_line() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("artifact_needle.rs"),
        "fn unrelated() {}\n",
    )
    .expect("filename hit");
    let matched = fixture.run(&["search", "--bm25", "artifact_needle"], "raw");
    assert!(
        matched.status.success(),
        "{}",
        String::from_utf8_lossy(&matched.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&matched.stdout).lines().next(),
        Some("File: artifact_needle.rs, Match: filename")
    );
    let excluded = fixture.run(
        &["search", "--bm25", "--exclude-filenames", "artifact_needle"],
        "raw",
    );
    assert_eq!(excluded.status.code(), Some(1));
    assert!(excluded.stdout.is_empty());
}

#[test]
fn raw_formats_frequency_and_token_budget_have_real_outputs() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("formats.rs"),
        "fn xml_marker() { let x = \"<value>&\"; xml_marker(); }\n",
    )
    .expect("format source");
    for format in [
        "plain",
        "terminal",
        "markdown",
        "json",
        "xml",
        "color",
        "outline",
        "outline-xml",
    ] {
        let output = fixture.run(
            &[
                "search",
                "--bm25",
                "--frequency",
                "--format",
                format,
                "xml_marker",
            ],
            "raw",
        );
        assert!(
            output.status.success(),
            "{format}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let rendered = String::from_utf8_lossy(&output.stdout);
        assert!(rendered.contains("formats.rs"), "{format}: {rendered}");
        match format {
            "json" => {
                let value: serde_json::Value =
                    serde_json::from_slice(&output.stdout).expect("JSON");
                assert_eq!(value[0]["occurrences"], 2);
            }
            "xml" => assert!(rendered.contains("&lt;value&gt;&amp;"), "{rendered}"),
            "outline-xml" => assert!(rendered.contains("<results>\n<hit "), "{rendered}"),
            "color" => assert!(rendered.contains("\x1b[36m"), "{rendered}"),
            _ => assert!(rendered.contains("2"), "{format}: {rendered}"),
        }
    }
    let too_few_tokens = fixture.run(&["search", "--bm25", "--max-tokens=1", "xml_marker"], "raw");
    assert_eq!(too_few_tokens.status.code(), Some(1));
    assert!(too_few_tokens.stdout.is_empty());
}

fn safe_test_root(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock before epoch")
        .as_nanos();
    PathBuf::from("/mnt/ssd/mirror-rootfs/home/obj/tmp")
        .join(format!("pbi-rs-{label}-{}-{nonce}", std::process::id()))
}

struct ScopeFixture {
    base: PathBuf,
    root: PathBuf,
    events: PathBuf,
}

impl ScopeFixture {
    fn new() -> Self {
        let _nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let base = safe_test_root("search-scope");
        let root = base.join("nested/invocation/root");
        fs::create_dir_all(&root).expect("create nested invocation root");
        let events = base.join("probe-events");
        Self { base, root, events }
    }

    fn run(&self, mode: &str) -> Output {
        self.run_args(mode, &["search", SCOPE_QUERY])
    }

    fn run_args(&self, _mode: &str, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run pbi-rs in nested invocation root")
    }

    fn run_session(&self, args: &[&str]) -> Output {
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .env("XDG_STATE_HOME", self.base.join("state"))
            .current_dir(&self.root)
            .args(args)
            .output()
            .expect("run pbi-rs with isolated state")
    }
}

impl Drop for ScopeFixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.base);
    }
}

#[test]
fn dispatch_version_alias_and_no_argument_diagnostic() {
    let fixture = Fixture::new();
    let version = fixture.run(&["--version"], "raw");
    let alias = fixture.run(&["-V"], "raw");
    assert!(alias.status.success());
    assert_eq!(alias.stdout, version.stdout);
    assert!(alias.stderr.is_empty());
    assert!(!fixture.root.join("probe-argv").exists());
}

#[test]
fn dispatch_no_argument_diagnostic() {
    let fixture = Fixture::new();
    let empty = fixture.run(&[], "raw");
    assert_eq!(empty.status.code(), Some(2));
    assert!(empty.stdout.is_empty());
    let stderr = String::from_utf8(empty.stderr).expect("utf-8");
    let mut lines = stderr.lines();
    assert_eq!(
        lines.next(),
        Some("pbi: question is required; interactive mode is disabled")
    );
    assert!(lines
        .next()
        .is_some_and(|line| line.starts_with("pbi-failure ")));
    assert!(lines.next().is_none());
    assert!(!fixture.root.join("probe-argv").exists());
}

#[test]
fn question_parity_message_rejects_unsupported_chat_tail_before_probe() {
    for tail in [
        vec!["--session", "resume-id"],
        vec!["--session=resume-id"],
        vec!["--session-id", "SID"],
        vec!["--session-id=SID"],
        vec!["--max-iterations", "1"],
        vec!["--max-iterations=1"],
        vec!["--allow-edit"],
        vec!["--enable-bash"],
        vec!["--web"],
        vec!["--trace-remote"],
        vec!["--trace-file", "trace.log"],
        vec!["--images", "shot.png"],
        vec!["--prompt", "extra"],
        vec!["--port", "9"],
        vec!["--debug"],
        vec!["--architecture-file", "arch.md"],
        vec!["--completion-prompt", "done"],
        vec!["--max-tokens", "123"],
        vec!["extra", "question"],
        vec!["--model-route", "http://example.invalid", "model", "handle"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["--message", "search option parity"];
        args.extend(tail);
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(String::from_utf8_lossy(&output.stderr).contains("unsupported"));
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn raw_session_pages_deduplicate_and_invalidate_changed_source() {
    let fixture = ScopeFixture::new();
    let source = fixture.root.join("blocks.rs");
    fs::write(&source, "fn marker() {}\n// gap\nfn marker() {}\n").expect("source");
    let args = [
        "search",
        "--bm25",
        "--format=json",
        "--merge-threshold=0",
        "--max-results=1",
        "--session=page",
        "marker",
    ];
    let first = fixture.run_session(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let one: serde_json::Value = serde_json::from_slice(&first.stdout).expect("first JSON");
    assert_eq!(one[0]["line"], 1);
    let second = fixture.run_session(&args);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let two: serde_json::Value = serde_json::from_slice(&second.stdout).expect("second JSON");
    assert_eq!(two[0]["line"], 3);
    let exhausted = fixture.run_session(&args);
    assert_eq!(exhausted.status.code(), Some(1));
    assert!(exhausted.stdout.is_empty());
    assert!(String::from_utf8_lossy(&exhausted.stderr).contains("exhausted"));
    fs::write(
        &source,
        "fn marker() { changed(); }\n// gap\nfn marker() {}\n",
    )
    .expect("modify source");
    let refreshed = fixture.run_session(&args);
    assert!(
        refreshed.status.success(),
        "{}",
        String::from_utf8_lossy(&refreshed.stderr)
    );
    let fresh: serde_json::Value = serde_json::from_slice(&refreshed.stdout).expect("fresh JSON");
    assert_eq!(fresh[0]["line"], 1);
    assert!(fresh[0]["snippet"]
        .as_str()
        .expect("snippet")
        .contains("changed"));
    let other_query = fixture.run_session(&[
        "search",
        "--bm25",
        "--format=json",
        "--max-results=1",
        "--session=page",
        "changed",
    ]);
    assert!(
        other_query.status.success(),
        "{}",
        String::from_utf8_lossy(&other_query.stderr)
    );
    let other: serde_json::Value = serde_json::from_slice(&other_query.stdout).expect("other JSON");
    assert_eq!(other[0]["line"], 1);
    let other_options = fixture.run_session(&[
        "search",
        "--bm25",
        "--format=json",
        "--max-results=1",
        "--session=page",
        "marker",
    ]);
    assert!(
        other_options.status.success(),
        "{}",
        String::from_utf8_lossy(&other_options.stderr)
    );
    let other: serde_json::Value =
        serde_json::from_slice(&other_options.stdout).expect("options JSON");
    assert_eq!(other[0]["line"], 1);
    assert_eq!(other[0]["end_line"], 3);
    let other_root = fixture.base.join("other-root");
    fs::create_dir(&other_root).expect("other root");
    fs::write(other_root.join("only.rs"), "fn marker() {}\n").expect("other source");
    let isolated = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("XDG_STATE_HOME", fixture.base.join("state"))
        .current_dir(&other_root)
        .args(args)
        .output()
        .expect("other root session");
    assert!(
        isolated.status.success(),
        "{}",
        String::from_utf8_lossy(&isolated.stderr)
    );
    let other: serde_json::Value = serde_json::from_slice(&isolated.stdout).expect("isolated JSON");
    assert_eq!(other[0]["file"], "only.rs");
    let state_dir = fixture.base.join("state/pbi-rs/search-sessions");
    assert_eq!(
        fs::metadata(&state_dir)
            .expect("state dir")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    for entry in fs::read_dir(&state_dir).expect("state entries") {
        let path = entry.expect("entry").path();
        assert_eq!(
            fs::metadata(&path)
                .expect("state file")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let bytes = fs::read(&path).expect("state bytes");
        assert!(!bytes.windows(6).any(|window| window == b"marker"));
    }
}

#[test]
fn raw_session_rejects_unsafe_ids_and_state_symlinks() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("match.rs"), "fn marker() {}\n").expect("source");
    for id in ["../outside", "", "a/b", ".", ".."] {
        let option = format!("--session={id}");
        let output = fixture.run_session(&["search", "--bm25", &option, "marker"]);
        assert_eq!(output.status.code(), Some(2), "{id:?}");
        assert!(output.stdout.is_empty());
    }
    let state = fixture.base.join("state");
    symlink(&fixture.root, &state).expect("state symlink");
    let output = fixture.run_session(&["search", "--bm25", "--session=safe", "marker"]);
    assert!(!output.status.success());
    assert!(output.stdout.is_empty());

    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("match.rs"), "fn marker() {}\n").expect("source");
    let args = ["search", "--bm25", "--session=safe", "marker"];
    let first = fixture.run_session(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let dir = fixture.base.join("state/pbi-rs/search-sessions");
    let state_file = fs::read_dir(&dir)
        .expect("state dir")
        .map(|entry| entry.expect("entry").path())
        .find(|path| path.extension().is_some_and(|ext| ext == "json"))
        .expect("state JSON");
    let outside = fixture.base.join("outside.txt");
    fs::write(&outside, "untouched").expect("outside");
    fs::remove_file(&state_file).expect("remove owned state");
    symlink(&outside, &state_file).expect("replace state with symlink");
    let second = fixture.run_session(&args);
    assert!(!second.status.success());
    assert!(second.stdout.is_empty());
    assert_eq!(
        fs::read_to_string(&outside).expect("outside content"),
        "untouched"
    );
}

#[test]
fn raw_session_concurrent_pages_remain_disjoint() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("blocks.rs"),
        "fn marker() {}\n// gap\nfn marker() {}\n",
    )
    .expect("source");
    let handles = (0..2)
        .map(|_| {
            let root = fixture.root.clone();
            let state = fixture.base.join("state");
            std::thread::spawn(move || {
                Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
                    .env_clear()
                    .env("XDG_STATE_HOME", state)
                    .current_dir(root)
                    .args([
                        "search",
                        "--timeout=30",
                        "--bm25",
                        "--format=json",
                        "--merge-threshold=0",
                        "--max-results=1",
                        "--session=parallel",
                        "marker",
                    ])
                    .output()
                    .expect("parallel session")
            })
        })
        .collect::<Vec<_>>();
    let mut lines = handles
        .into_iter()
        .map(|handle| {
            let output = handle.join().expect("child thread");
            assert!(
                output.status.success(),
                "{}",
                String::from_utf8_lossy(&output.stderr)
            );
            let rows: serde_json::Value = serde_json::from_slice(&output.stdout).expect("JSON");
            rows[0]["line"].as_u64().expect("line")
        })
        .collect::<Vec<_>>();
    lines.sort();
    assert_eq!(lines, [1, 3]);
}

#[test]
fn raw_session_budget_advances_only_emitted_results() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("blocks.rs"),
        "fn marker() {}\n// gap\nfn marker() {}\n",
    )
    .expect("source");
    let unpaged = fixture.run_session(&[
        "search",
        "--bm25",
        "--format=json",
        "--merge-threshold=0",
        "--max-results=1",
        "marker",
    ]);
    assert!(unpaged.status.success());
    assert!(
        !fixture.base.join("state").exists(),
        "stateless search wrote session data"
    );
    let budget = format!("--max-bytes={}", unpaged.stdout.len());
    let args = [
        "search",
        "--bm25",
        "--format=json",
        "--merge-threshold=0",
        "--max-results=2",
        &budget,
        "--session=budget",
        "marker",
    ];
    let first = fixture.run_session(&args);
    assert!(
        first.status.success(),
        "{}",
        String::from_utf8_lossy(&first.stderr)
    );
    let one: serde_json::Value = serde_json::from_slice(&first.stdout).expect("first JSON");
    assert_eq!(one.as_array().expect("first rows").len(), 1);
    assert_eq!(one[0]["line"], 1);
    assert!(String::from_utf8_lossy(&first.stderr).contains("truncated"));
    let second = fixture.run_session(&args);
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    let two: serde_json::Value = serde_json::from_slice(&second.stdout).expect("second JSON");
    assert_eq!(two[0]["line"], 3);
}

#[test]
fn raw_backend_only_options_are_refused_instead_of_ignored() {
    let fixture = Fixture::new();
    for args in [
        vec!["search", "--bm25", "--reranker=bert", "search_option"],
        vec!["search", "--bm25", "--reranker", "search_option"],
        vec!["search", "--bm25", "--question", "another", "search_option"],
    ] {
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
    }
}

#[test]
fn search_format_invalid_duplicates_and_verified_requests_refuse_before_probe() {
    for raw in [false, true] {
        for tail in [
            vec!["--format"],
            vec!["-o"],
            vec!["--format="],
            vec!["-o="],
            vec!["--format", "--"],
            vec!["-o", "--bm25"],
            vec!["--format", ""],
            vec!["--format=JSON"],
            vec!["--format=unknown"],
            vec!["-oxml", "--format=json"],
            vec!["--format=plain", "-o", "json"],
        ] {
            let fixture = Fixture::new();
            let mut args = vec!["search", "search option parity"];
            if raw {
                args.push("--bm25");
            }
            args.extend(tail);
            let output = fixture.run(&args, "raw");
            assert_eq!(output.status.code(), Some(2), "{args:?}");
            assert!(output.stdout.is_empty());
            assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
        }
    }
    // Legacy appends --format plain: installed Probe rejects even an explicit plain.
    for format in ["json", "plain", "outline-xml"] {
        let fixture = Fixture::new();
        let output = fixture.run(
            &["search", "--format", format, "search option parity"],
            "evidence",
        );
        assert_eq!(output.status.code(), Some(2));
        assert!(String::from_utf8_lossy(&output.stderr)
            .contains("--format cannot be used multiple times"));
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn invalid_filter_operands_stop_before_probe() {
    for args in [
        vec!["search", "--language"],
        vec!["search", "-l"],
        vec!["search", "--ignore"],
        vec!["search", "-i"],
        vec!["search", "--language", "--ignore", "query"],
        vec!["search", "--ignore", "--", "query"],
        vec!["search", "--language=", "query"],
        vec!["search", "--ignore=", "query"],
        vec!["search", "--language", "not-a-language", "query"],
        vec!["search", "-l", "rust", "-l", "python", "query"],
        vec!["search", "--format", "json", "query"],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "evidence");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty());
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn invalid_or_missing_numeric_options_stop_before_probe() {
    let fixture = Fixture::new();
    for (args, expected_error) in [
        (&["search", "--timeout"][..], "--timeout requires a value"),
        (
            &["search", "--max-results"][..],
            "--max-results requires a value",
        ),
        (
            &["search", "--timeout=fast", "search option parity"][..],
            "--timeout must be a non-negative integer",
        ),
        (
            &["search", "--max-results=0", "search option parity"][..],
            "--max-results must be a positive integer",
        ),
    ] {
        let output = fixture.run(args, "evidence");
        assert_eq!(output.status.code(), Some(2));
        assert!(
            String::from_utf8_lossy(&output.stderr).contains(expected_error),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists());
    }
}

#[test]
fn raw_search_rejects_repeated_safe_flags_before_probe() {
    for tail in [
        vec!["--files-only", "--files-only"],
        vec!["-f", "-f"],
        vec!["--files-only", "-f"],
        vec!["--exact", "--exact"],
        vec!["-e", "-e"],
        vec!["--exact", "-e"],
        vec!["--frequency", "--frequency"],
        vec!["-s", "-s"],
        vec!["--frequency", "-s"],
        vec!["--exclude-filenames", "--exclude-filenames"],
        vec!["-n", "-n"],
        vec!["--exclude-filenames", "-n"],
        vec!["--strict-elastic-syntax", "--strict-elastic-syntax"],
    ] {
        let fixture = Fixture::new();
        let mut args = vec!["search", "--bm25"];
        args.extend(tail);
        args.extend(["--", "parse_search"]);
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("cannot be used multiple times"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
    }
}

#[test]
fn safe_flags_without_bm25_and_deferred_flags_stop_before_probe() {
    for args in [
        vec![
            "search",
            "--files-only",
            "--format",
            "json",
            "--",
            "parse_search",
        ],
        vec!["search", "-f", "--", "parse_search"],
        vec!["search", "--exact", "--", "parse_search"],
        vec!["search", "-e", "--", "parse_search"],
        vec!["search", "--frequency", "--", "parse_search"],
        vec!["search", "-s", "--", "parse_search"],
        vec!["search", "--exclude-filenames", "--", "parse_search"],
        vec!["search", "-n", "--", "parse_search"],
        vec!["search", "--strict-elastic-syntax", "--", "parse_search"],
        vec!["search", "--bm25", "--allow-tests", "--", "parse_search"],
        vec!["search", "--bm25", "--no-gitignore", "--", "parse_search"],
        vec!["search", "--bm25", "--no-merge", "--", "parse_search"],
        vec!["search", "--bm25", "--lsp", "--", "parse_search"],
        vec![
            "search",
            "--bm25",
            "--not-a-real-flag",
            "--",
            "parse_search",
        ],
        vec![
            "search",
            "--bm25",
            "--files-only=json",
            "--",
            "parse_search",
        ],
    ] {
        let fixture = Fixture::new();
        let output = fixture.run(&args, "raw");
        assert_eq!(output.status.code(), Some(2), "{args:?}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(
            String::from_utf8_lossy(&output.stderr).contains("unsupported search option"),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!fixture.root.join("probe-argv").exists(), "{args:?}");
    }
}

#[test]
fn root_scope_root_documents_do_not_hide_nested_source() {
    let fixture = ScopeFixture::new();
    for index in 0..17 {
        fs::write(
            fixture.root.join(format!("noise-{index:02}.md")),
            "not source\n",
        )
        .expect("write root noise");
    }
    fs::create_dir_all(fixture.root.join("src/nested")).expect("create nested source directory");
    fs::write(
        fixture.root.join("src/nested/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write depth-two source after noise");
    let output = fixture.run("saturated");
    assert!(
        output.status.success(),
        "selected-root contents stay searchable"
    );
    assert_eq!(compact_locations(&output), ["src/nested/mod.rs:1"]);
}

#[test]
fn root_scope_depth_three_source_directory() {
    let fixture = ScopeFixture::new();
    fs::create_dir_all(fixture.root.join("src/nested/deeper")).expect("depth three");
    fs::write(
        fixture.root.join("src/nested/deeper/mod.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("write depth-three source");
    let output = fixture.run("deep");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("src/nested/deeper/mod.rs:1"), "{stdout}");
}

#[test]
fn root_scope_keeps_root_docs_and_config() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("README.md"),
        format!("display_relative lives here /* {SCOPE_QUERY} */\n"),
    )
    .expect("write root doc");
    fs::write(fixture.root.join("Cargo.toml"), "[package]\nname = \"x\"\n").expect("config");
    let output = fixture.run("rootdoc");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(stdout.contains("README.md:1"), "{stdout}");
}

#[test]
fn root_scope_user_ignore_skips_fallback() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join("chosen.rs"), "fn display_relative() {}\n").expect("rs");
    fs::write(
        fixture.root.join("chosen.py"),
        "def display_relative(): pass\n",
    )
    .expect("py");
    let output = fixture.run_args(
        "ignored",
        &["search", "--ignore=*.rs", "--ignore=*.py", SCOPE_QUERY],
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stdout.is_empty() && !stdout.contains("Coverage: complete"),
        "stdout={stdout} stderr={stderr}",
    );
    assert_eq!(output.status.code(), Some(1));
    let mut lines = stderr.lines();
    assert_eq!(lines.next(), Some("pbi: no source locations found"));
    assert!(lines
        .next()
        .is_some_and(|line| line.starts_with("pbi-failure ")));
    assert!(lines.next().is_none());
}

#[test]
fn caller_failure_receipt_is_automatic_and_bounded() {
    let fixture = Fixture::new();
    let secret = "sk-live-failure-canary";
    let query = format!("missing_symbol_{secret}");
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("LOCAL_ROUTER_API_KEY", secret)
        .current_dir(&fixture.root)
        .args(["search", "--timeout", "8", &query])
        .output()
        .expect("controlled miss");
    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    assert!(
        stderr.starts_with("pbi: no source locations found\n"),
        "{stderr}"
    );
    let receipt = stderr
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("pbi-failure "))
        .expect("receipt line");
    let fields: std::collections::HashMap<&str, &str> = receipt
        .split(' ')
        .filter_map(|part| part.split_once('='))
        .collect();
    assert_eq!(fields["rc"], "1");
    assert_eq!(fields["stage"], "initial_verify");
    assert_eq!(fields["stage_status"], "no_source");
    assert_eq!(fields["candidates"], "unknown");
    assert_eq!(fields["ranges"], "0");
    assert_eq!(fields["admission"], "unknown");
    assert_eq!(fields["deadline_s"], "8");
    assert!(fields["elapsed_ms"].chars().all(|c| c.is_ascii_digit()));
    assert_eq!(fields["cwd"], "[REDACTED]");
    assert_eq!(fields["exe"], "[REDACTED]");
    assert_eq!(fields["argv0"], "[REDACTED]");
    assert!(fields["argv"].contains("search"));
    assert!(fields["argv"].contains("--timeout"));
    assert_eq!(fields["argv_count"], "4");
    assert!(
        !stderr.contains(secret),
        "failure output kept the query secret"
    );
    assert!(!stderr.contains("LOCAL_ROUTER_API_KEY"));
    assert!(!stderr.contains(fixture.root.to_str().unwrap()));
}

#[test]
fn automatic_failure_redacts_opaque_caller_values() {
    let fixture = Fixture::new();
    let nested = fixture.root.join("weird path");
    fs::create_dir(&nested).expect("nested cwd");
    let canaries = [
        "opaque-canary-query-7f3a",
        "opaque-canary-flag-eq-91c2",
        "opaque-canary-flag-value-44de",
        "opaque-canary-url-a81b",
        "opaque-canary-password-c03e",
        "opaque-canary-endpoint-55aa",
        "opaque-canary-path-d17f",
        "opaque-canary-cwd-e90b",
    ];
    let query = format!(
        "{} https://user:{}@endpoint.example/v1?token={} path/{}\nline\r\t\u{1b}uni-Δ",
        canaries[0], canaries[4], canaries[5], canaries[6]
    );
    let oversized = "Z".repeat(300);
    let bad_flag = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(&nested)
        .args(["search", &format!("--bogus={}", canaries[1])])
        .output()
        .expect("bad flag");
    let miss = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("LOCAL_ROUTER_API_KEY", canaries[5])
        .current_dir(&nested)
        .args([
            "search",
            &format!("--ignore={}", canaries[1]),
            "--language",
            "rs",
            "--timeout",
            "8",
            &query,
            &oversized,
        ])
        .output()
        .expect("controlled miss");
    for output in [&bad_flag, &miss] {
        assert!(output.stdout.is_empty(), "stdout changed on failure");
        let stderr = String::from_utf8(output.stderr.clone()).expect("utf-8 stderr");
        let controls: Vec<u32> = stderr
            .chars()
            .filter(|ch| ch.is_control() && *ch != '\n')
            .map(|ch| ch as u32)
            .collect();
        assert!(
            controls.is_empty(),
            "failure output kept control code points {controls:?}"
        );
        assert_eq!(
            stderr.trim_end_matches('\n').lines().count(),
            2,
            "failure output was not two lines"
        );
        for canary in &canaries {
            assert!(
                !stderr.contains(canary),
                "failure output kept an opaque caller value"
            );
        }
        assert!(
            !stderr.contains(&oversized),
            "failure output kept an oversized argument"
        );
        assert!(
            !stderr.contains("weird path"),
            "failure output kept the cwd"
        );
        assert!(!stderr.contains("LOCAL_ROUTER_API_KEY"));
        assert!(
            stderr.contains("[REDACTED]"),
            "failure output had no redaction marker"
        );
    }
    assert_eq!(bad_flag.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&bad_flag.stderr).starts_with("pbi-rs: "),
        "usage class missing"
    );
    assert_eq!(miss.status.code(), Some(1));
    let stderr = String::from_utf8(miss.stderr).expect("utf-8 stderr");
    assert!(
        stderr.starts_with("pbi: no source locations found\n"),
        "failure class missing"
    );
    let receipt = stderr
        .lines()
        .nth(1)
        .and_then(|line| line.strip_prefix("pbi-failure "))
        .expect("receipt line");
    let fields: std::collections::HashMap<&str, &str> = receipt
        .split(' ')
        .filter_map(|part| part.split_once('='))
        .collect();
    assert_eq!(fields["rc"], "1");
    assert_eq!(fields["stage"], "initial_verify");
    assert_eq!(fields["stage_status"], "no_source");
    assert_eq!(fields["candidates"], "unknown");
    assert_eq!(fields["ranges"], "0");
    assert_eq!(fields["admission"], "unknown");
    assert_eq!(fields["deadline_s"], "8");
    assert!(fields["elapsed_ms"].chars().all(|c| c.is_ascii_digit()));
    assert_eq!(fields["cwd"], "[REDACTED]");
    assert_eq!(fields["exe"], "[REDACTED]");
    assert_eq!(fields["argv0"], "[REDACTED]");
    assert_eq!(fields["argv_count"], "8");
    assert_eq!(
        fields["argv"],
        "search,--ignore,[REDACTED],--language,[REDACTED],--timeout,8,[REDACTED],[REDACTED]"
    );
}

#[test]
fn shared_failure_receipt_redacts_operand_roles() {
    let fixture = Fixture::new();
    let numeric = "98765432109876543210";
    let oversized = "9".repeat(8193);
    let cases = [
        vec![numeric],
        vec!["--message", "--timeout", numeric],
        vec!["search", numeric],
        vec!["search", "--bm25", numeric],
        vec!["search", "--regex", numeric],
        vec!["search", "--regex", "[", numeric],
        vec!["search", "--regex", &oversized],
        vec!["search", "--regex", "--", "--session"],
        vec!["search", "--", "--timeout", numeric],
        vec!["search", "--session", numeric, "missing"],
        vec!["search", "--unknown", "--timeout", numeric],
        vec!["--model-name", numeric, "missing"],
        vec!["--model-route", numeric, numeric, numeric, "missing"],
        vec!["extract", numeric],
        vec!["extract", "--timeout", numeric],
        vec!["symbols", numeric],
        vec!["symbols", "--timeout", numeric],
        vec!["search", "--timeout", &oversized, "missing"],
        vec!["search", "--max-results", &oversized, "missing"],
    ];
    for args in cases {
        let output = fixture.run(&args, "privacy");
        assert!(!output.status.success(), "controlled failure required");
        assert!(output.stdout.is_empty(), "failure stdout must remain empty");
        let stderr = String::from_utf8(output.stderr).expect("static UTF-8 failure");
        assert!(failure_fields(&stderr).contains_key("argv"));
        assert!(!stderr.contains(numeric), "numeric operand leaked");
        assert!(!stderr.contains(&oversized), "oversized operand leaked");
        assert!(stderr.len() < 2048, "failure receipt exceeded bound");
        if args.contains(&"--") {
            let argv = failure_fields(&stderr)["argv"];
            assert!(!argv.contains("--session"), "literal option operand leaked");
            assert!(!argv.contains("--timeout"), "literal numeric option leaked");
        }
    }
}

#[test]
fn shared_failure_receipt_retains_valid_public_bounds() {
    let fixture = Fixture::new();
    for args in [
        vec!["search", "--timeout", "0", "--max-results", "2", "missing"],
        vec!["search", "--timeout=0", "--max-results=2", "missing"],
        vec!["--timeout", "0", "missing"],
        vec![
            "extract",
            "fixture.rs:1",
            "--timeout",
            "0",
            "--max-bytes",
            "2",
        ],
    ] {
        let output = fixture.run(&args, "privacy");
        assert_eq!(output.status.code(), Some(1), "deadline failure required");
        let stderr = String::from_utf8(output.stderr).expect("static UTF-8 failure");
        let fields = failure_fields(&stderr);
        assert_eq!(fields.get("deadline_s"), Some(&"0"));
        assert!(
            fields["argv"].contains("--timeout,0"),
            "public timeout lost"
        );
        if args.iter().any(|arg| arg.starts_with("--max-results")) {
            assert!(
                fields["argv"].contains("--max-results,2"),
                "public result bound lost"
            );
        }
        assert!(!stderr.contains("missing"), "query leaked");
        assert!(!stderr.contains("fixture.rs:1"), "position leaked");
    }
}

#[test]
fn non_utf8_argument_fails_closed() {
    let fixture = Fixture::new();
    let argument = OsString::from_vec(b"--timeout=98765432109876543210\xff".to_vec());
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(&fixture.root)
        .arg("search")
        .arg(argument)
        .arg("missing_symbol")
        .output()
        .expect("non-utf8 argv");
    let stderr = String::from_utf8(output.stderr.clone()).expect("static utf-8 stderr");
    assert_eq!(output.status.code(), Some(2), "non-UTF-8 must fail closed");
    assert!(output.stdout.is_empty());
    assert!(stderr.starts_with("pbi-rs: arguments must be UTF-8\n"));
    assert!(stderr.contains("deadline_s=unknown"));
    assert!(!stderr.contains('\u{fffd}'));
    assert!(
        !stderr.contains("98765432109876543210"),
        "numeric bytes leaked"
    );
    assert!(stderr.len() < 512, "non-UTF-8 receipt exceeded bound");
    assert_eq!(
        stderr.lines().count(),
        2,
        "static receipt must be two lines"
    );
}

#[test]
fn equals_timeout_receipt_uses_enforced_deadline() {
    let fixture = Fixture::new();
    let cases = [
        (vec!["search", "--timeout=0", "missing_symbol"], "1", "0"),
        (vec!["search", "--timeout", "0", "missing_symbol"], "1", "0"),
        (
            vec!["search", "--bm25", "--timeout=0", "missing_symbol"],
            "1",
            "0",
        ),
        (vec!["--timeout=0", "missing_symbol"], "1", "0"),
        (vec!["--timeout", "0", "missing_symbol"], "1", "0"),
        (
            vec!["search", "--timeout=4", "--timeout", "0", "missing_symbol"],
            "2",
            "8",
        ),
        (
            vec!["search", "--timeout", "0", "--timeout=4", "missing_symbol"],
            "2",
            "8",
        ),
        (
            vec!["--timeout=0", "--timeout", "4", "missing_symbol"],
            "2",
            "90",
        ),
        (vec!["search", "--timeout=fast", "missing_symbol"], "2", "8"),
        (vec!["--timeout=fast", "missing_symbol"], "2", "90"),
    ];
    for (args, code, deadline) in cases {
        let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .current_dir(&fixture.root)
            .args(&args)
            .output()
            .expect("run timeout form");
        let stderr = String::from_utf8(output.stderr).expect("utf-8");
        assert_eq!(
            output.status.code(),
            Some(code.parse().unwrap()),
            "{args:?} {stderr}"
        );
        assert!(output.stdout.is_empty(), "{args:?}");
        let receipt = stderr
            .lines()
            .find_map(|line| line.strip_prefix("pbi-failure "))
            .unwrap_or_else(|| panic!("missing receipt for {args:?}: {stderr}"));
        let fields: std::collections::HashMap<&str, &str> = receipt
            .split(' ')
            .filter_map(|part| part.split_once('='))
            .collect();
        assert_eq!(fields["deadline_s"], deadline, "{args:?} {stderr}");
        assert!(!stderr.contains("fast"), "{args:?} {stderr}");
    }
}

#[test]
fn zero_timeout_answer_stays_at_initial_search() {
    let fixture = Fixture::new();
    fs::write(
        fixture.root.join("decision.rs"),
        "fn native_json_fallback_eligible() -> bool { false }\n",
    )
    .expect("source");
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("PBI_RS_ADK_ENABLE", "0")
        .current_dir(&fixture.root)
        .args([
            "--timeout=0",
            "How does native_json_fallback_eligible decide whether to retry?",
        ])
        .output()
        .expect("zero timeout question");
    let stderr = String::from_utf8(output.stderr).expect("utf-8");
    let receipt_lines = stderr
        .lines()
        .filter(|line| line.starts_with("pbi-failure "))
        .count();
    assert_eq!(
        output.status.code(),
        Some(1),
        "status={:?}",
        output.status.code()
    );
    assert!(
        output.stdout.is_empty(),
        "stdout_bytes={}",
        output.stdout.len()
    );
    let fields = failure_fields(&stderr);
    assert_eq!(fields["stage"], "initial_search");
    assert_eq!(fields["stage_status"], "deadline");
    assert_eq!(fields["candidates"], "unknown");
    assert_eq!(fields["ranges"], "unknown");
    assert_eq!(fields["admission"], "unknown");
    assert_eq!(fields["deadline_s"], "0");
    assert_eq!(receipt_lines, 1, "receipt_lines={receipt_lines}");
    assert!(
        !stderr.contains("native_json_fallback_eligible"),
        "stderr leaked the queried symbol"
    );
}

fn failure_fields(stderr: &str) -> std::collections::HashMap<&str, &str> {
    stderr
        .lines()
        .find_map(|line| line.strip_prefix("pbi-failure "))
        .expect("receipt")
        .split(' ')
        .filter_map(|part| part.split_once('='))
        .collect()
}

#[test]
fn successful_search_stdout_omits_failure_receipt() {
    let fixture = Fixture::new();
    let output = fixture.run(&["search", "search_option"], "verified");
    assert!(output.status.success(), "success path failed");
    assert_eq!(compact_locations(&output), ["fixture.rs:1"]);
    assert!(
        !String::from_utf8_lossy(&output.stderr).contains("pbi-failure"),
        "success printed a failure receipt"
    );
}

#[test]
fn root_scope_rejects_early_display_stem() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/decoy.rs"),
        "fn show(path: &std::path::Path) {\n    let _ = path.display();\n}\n",
    )
    .expect("decoy");
    let output = fixture.run_args("decoy", &["search", "SourceLocation display_relative"]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stdout.contains("Coverage: complete"),
        "stdout={stdout} stderr={stderr}"
    );
    assert!(!stdout.contains("decoy.rs"), "stdout={stdout}");
}

#[test]
fn root_scope_many_children_searches_complete_root() {
    let fixture = ScopeFixture::new();
    for index in 0..17 {
        fs::create_dir_all(fixture.root.join(format!("pkg-{index:02}/src"))).expect("pkg");
        fs::write(
            fixture.root.join(format!("pkg-{index:02}/src/lib.rs")),
            "fn other() {}\n",
        )
        .expect("src");
    }
    fs::create_dir_all(fixture.root.join("pkg-zz/src")).expect("late");
    fs::write(
        fixture.root.join("pkg-zz/src/lib.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("late source");
    let output = fixture.run("overflow");
    assert!(
        output.status.success(),
        "bounded whole-root traversal must succeed"
    );
    assert_eq!(compact_locations(&output), ["pkg-zz/src/lib.rs:1"]);
}

#[test]
fn root_boundary_directory_glob_excludes_nested_source() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    fs::write(fixture.root.join(".gitignore"), "src/hidden.rs\n").expect("gitignore");
    fs::write(
        fixture.root.join("src/hidden.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("hidden");
    let output = fixture.run_args("filtered", &["search", "--ignore=src/**", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        !stdout.contains("src/lib.rs") && !stdout.contains("Coverage: complete"),
        "directory glob must exclude nested source; stdout={stdout}",
    );
    assert_eq!(output.status.code(), Some(1));
}

#[test]
fn root_boundary_fifo_does_not_block_planning() {
    let fixture = ScopeFixture::new();
    let fifo = fixture.root.join("events.pipe");
    std::process::Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("mkfifo");
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let started = Instant::now();
    let output = fixture.run_args("filtered", &["search", "--timeout=1", SCOPE_QUERY]);
    assert!(
        started.elapsed() < Duration::from_secs(3),
        "planning opened a fifo and blocked"
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success() && stdout.contains("kept.rs"),
        "stdout={stdout} stderr={}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn root_boundary_rejects_absent_member_and_symbol_prefix() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "impl SourceLocation {\n    fn new() {}\n    fn display_relative(&self) {}\n}\n",
    )
    .expect("lib");
    let member = fixture.run_args("filtered", &["search", "SourceLocation nonexistent_member"]);
    let member_out = String::from_utf8_lossy(&member.stdout);
    assert!(
        !member_out.contains("Coverage: complete"),
        "absent member accepted via owner; stdout={member_out}"
    );
    let prefix = fixture.run_args("filtered", &["search", "display_relati"]);
    let prefix_out = String::from_utf8_lossy(&prefix.stdout);
    assert!(
        !prefix_out.contains("Coverage: complete"),
        "symbol prefix accepted as exact; stdout={prefix_out}"
    );
}

#[test]
fn root_boundary_numeric_zero_spellings_share_fallback() {
    let fixture = ScopeFixture::new();
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn display_relative() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let mut codes = Vec::new();
    for spelling in ["0", "+0", "000"] {
        let output = fixture.run_args(
            "filtered",
            &[
                "search",
                &format!("--merge-threshold={spelling}"),
                SCOPE_QUERY,
            ],
        );
        codes.push(output.status.code());
    }
    assert!(
        codes.windows(2).all(|pair| pair[0] == pair[1]),
        "numeric spellings changed fallback: {codes:?}"
    );
}

#[test]
fn compact_output_keeps_ranked_source_blocks_and_both_files() {
    let fixture = ScopeFixture::new();
    fs::create_dir(fixture.root.join("src")).expect("src");
    fs::write(
        fixture.root.join("src/lib.rs"),
        "impl SourceLocation {\n    fn display_relative(&self) {}\n}\n",
    )
    .expect("member");
    let member = fixture.run_args("saturated", &["search", "SourceLocation display_relative"]);
    assert_eq!(
        member.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&member.stderr)
    );
    assert_eq!(compact_locations(&member), ["src/lib.rs:1-2"]);

    fs::write(fixture.root.join("b.rs"), "fn parse_search() {}\n").expect("b");
    fs::write(fixture.root.join("a.rs"), "fn parse_search() {}\n").expect("a");
    let both = fixture.run_args("files", &["search", "where is parse_search"]);
    assert_eq!(
        both.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&both.stderr)
    );
    assert_eq!(compact_locations(&both), ["a.rs:1", "b.rs:1"]);

    let repeated = ScopeFixture::new();
    fs::write(
        repeated.root.join("twice.rs"),
        "fn parse_search() {}\nfn parse_search() {}\n",
    )
    .expect("twice");
    let twice = repeated.run_args("files", &["search", "where is parse_search"]);
    assert_eq!(
        twice.status.code(),
        Some(0),
        "{}",
        String::from_utf8_lossy(&twice.stderr)
    );
    assert_eq!(compact_locations(&twice), ["twice.rs:1-2"]);
}

#[test]
fn native_search_honors_gitignore_with_many_children_and_symlink() {
    let fixture = ScopeFixture::new();
    fs::write(fixture.root.join(".gitignore"), "secret.rs\n").expect("gitignore");
    fs::write(
        fixture.root.join("secret.rs"),
        format!("fn hidden_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("ignored");
    fs::write(
        fixture.root.join("kept.rs"),
        format!("fn kept_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("kept");
    let outside = fixture.base.join("outside.rs");
    fs::write(
        &outside,
        format!("fn outside_match() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("outside");
    symlink(&outside, fixture.root.join("linked.rs")).expect("link");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        output.status.success(),
        "visible selected-root source should succeed"
    );
    assert!(
        stdout.contains("kept.rs")
            && !stdout.contains("secret.rs")
            && !stdout.contains("linked.rs"),
        "ignored and symlinked sources must stay excluded"
    );
    assert!(!fixture.events.is_file());
    for index in 0..16 {
        fs::write(
            fixture.root.join(format!("cap-{index:02}.rs")),
            "fn decoy() {}\n",
        )
        .expect("decoy");
    }
    let expanded = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    let expanded_stdout = String::from_utf8_lossy(&expanded.stdout);
    assert!(
        expanded.status.success(),
        "selected-root contents beyond 16 entries must remain searchable"
    );
    assert!(
        expanded_stdout.contains("kept.rs")
            && !expanded_stdout.contains("secret.rs")
            && !expanded_stdout.contains("linked.rs"),
        "ignored and symlinked sources must stay excluded after full-root traversal"
    );
}

#[test]
fn native_search_refuses_symlinked_gitignore_outside_root() {
    let fixture = ScopeFixture::new();
    let outside = fixture.base.join("outside-ignore");
    fs::write(&outside, "secret.rs\n").expect("outside ignore file");
    symlink(&outside, fixture.root.join(".gitignore")).expect("linked ignore file");
    fs::write(
        fixture.root.join("secret.rs"),
        format!("fn secret() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not read the repository"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

#[test]
fn native_search_respects_nested_gitignore_and_negation() {
    let fixture = ScopeFixture::new();
    let src = fixture.root.join("src");
    fs::create_dir(&src).expect("source directory");
    fs::write(
        fixture.root.join(".gitignore"),
        "src/*.rs\n!src/kept.rs\n!src/hidden.rs\n",
    )
    .expect("root gitignore");
    fs::write(src.join(".gitignore"), "hidden.rs\n").expect("nested gitignore");
    for name in ["kept.rs", "hidden.rs", "blocked.rs"] {
        fs::write(
            src.join(name),
            format!("fn matching() {{ /* {SCOPE_QUERY} */ }}\n"),
        )
        .expect("fixture source");
    }
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(output.status.success(), "{stdout} {:?}", output.stderr);
    assert!(stdout.contains("src/kept.rs"), "{stdout}");
    assert!(
        !stdout.contains("hidden.rs") && !stdout.contains("blocked.rs"),
        "{stdout}"
    );
}

#[test]
fn native_search_hard_denies_whitelisted_hidden_components() {
    const HIDDEN_MARKER: &str = "synthetic_issue326_hidden_marker";
    const ORDINARY_MARKER: &str = "ordinary_issue326_whitelist_marker";
    const NESTED_MARKER: &str = "nested_issue326_whitelist_marker";

    let fixture = ScopeFixture::new();
    let root = &fixture.root;
    fs::create_dir_all(root.join(".private")).expect("hidden directory");
    fs::create_dir_all(root.join("dir")).expect("nested directory");
    fs::write(
        root.join(".gitignore"),
        "*.rs\n!.env\n!dir/\n!dir/.env.local\n!.private/\n!.private/**\n!visible.rs\n!visible_candidate.rs\n",
    )
    .expect("root gitignore");
    fs::write(root.join(".env"), HIDDEN_MARKER).expect("synthetic env fixture");
    fs::write(root.join("dir/.env.local"), HIDDEN_MARKER).expect("synthetic nested env fixture");
    fs::write(
        root.join(".private/issue326_hidden_candidate.rs"),
        "fn issue326_hidden_candidate() {}\n",
    )
    .expect("synthetic hidden candidate");
    fs::write(root.join("visible.rs"), ORDINARY_MARKER).expect("ordinary whitelist fixture");
    fs::write(
        root.join("visible_candidate.rs"),
        "fn issue326_hidden_candidate() {}\n",
    )
    .expect("ordinary candidate fixture");
    fs::write(root.join("dir/.gitignore"), "*.rs\n!kept.rs\nblocked.rs\n")
        .expect("nested gitignore");
    fs::write(root.join("dir/kept.rs"), NESTED_MARKER).expect("nested whitelist fixture");
    fs::write(root.join("dir/blocked.rs"), NESTED_MARKER).expect("nested ignored fixture");

    for args in [
        vec!["search", HIDDEN_MARKER],
        vec!["search", "--bm25", HIDDEN_MARKER],
        vec!["search", "--bm25", "--format=json", HIDDEN_MARKER],
    ] {
        let output = fixture.run_args("unused", &args);
        assert_eq!(output.status.code(), Some(1));
        assert!(output.stdout.is_empty());
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(!stdout.contains(HIDDEN_MARKER) && !stderr.contains(HIDDEN_MARKER));
        assert!(!stdout.contains(".env") && !stderr.contains(".env"));
        assert!(!stdout.contains(".private") && !stderr.contains(".private"));
    }

    let ordinary = fixture.run_args("unused", &["search", ORDINARY_MARKER]);
    assert!(ordinary.status.success());
    assert!(String::from_utf8_lossy(&ordinary.stdout).contains("visible.rs"));

    let nested = fixture.run_args("unused", &["search", NESTED_MARKER]);
    let nested_stdout = String::from_utf8_lossy(&nested.stdout);
    assert!(nested.status.success());
    assert!(nested_stdout.contains("dir/kept.rs"));
    assert!(!nested_stdout.contains("blocked.rs"));

    let candidate = fixture.run_args("unused", &["search", "issue326_hidden_candidate"]);
    let candidate_output = String::from_utf8_lossy(&candidate.stdout);
    assert!(!candidate_output.contains(".private"));
    assert!(candidate_output.contains("visible_candidate.rs"));
}

#[test]
fn native_search_finds_checkout_source() {
    let output = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .current_dir(env!("CARGO_MANIFEST_DIR"))
        .args(["search", "display_relative"])
        .output()
        .expect("search checkout");
    assert!(
        output.status.success(),
        "stdout={} stderr={}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("src/lib.rs"));
}

#[test]
fn stage_timing_is_opt_in_bounded_and_content_free() {
    let fixture = Fixture::new();
    let query = "private_stage_query_marker";
    fs::write(
        fixture.root.join("private_stage.rs"),
        format!("fn {query}() {{}}\n"),
    )
    .expect("source");
    let ordinary = fixture.run(&["search", query], "unused");
    assert!(ordinary.status.success());
    assert!(ordinary.stderr.is_empty());

    let traced = Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
        .env_clear()
        .env("PBI_RS_STAGE_TIMING", "1")
        .env("LOCAL_ROUTER_API_KEY", "private-credential-canary")
        .current_dir(&fixture.root)
        .args(["search", query])
        .output()
        .expect("traced search");
    assert!(traced.status.success());
    let stderr = String::from_utf8(traced.stderr).expect("UTF-8 telemetry");
    let lines = stderr.lines().collect::<Vec<_>>();
    assert!((2..=8).contains(&lines.len()), "{stderr}");
    assert!(lines.iter().all(|line| line.starts_with("pbi-stage ")));
    assert!(!stderr.contains(query));
    assert!(!stderr.contains("private_stage.rs"));
    assert!(!stderr.contains("private-credential-canary"));
    assert!(!stderr.contains(&fixture.root.to_string_lossy().to_string()));
}

#[test]
fn native_search_refuses_symlinked_nested_gitignore_outside_root() {
    let fixture = ScopeFixture::new();
    let src = fixture.root.join("src");
    fs::create_dir(&src).expect("source directory");
    let outside = fixture.base.join("outside-ignore");
    fs::write(&outside, "secret.rs\n").expect("outside ignore file");
    symlink(&outside, src.join(".gitignore")).expect("linked ignore file");
    fs::write(
        src.join("secret.rs"),
        format!("fn secret() {{ /* {SCOPE_QUERY} */ }}\n"),
    )
    .expect("source");
    let output = fixture.run_args("unused", &["search", SCOPE_QUERY]);
    assert_eq!(output.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("could not read the repository"),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(output.stdout.is_empty());
}

fn context_window(name: &str, marker: &str, width: usize) -> String {
    // Longer windows lose to the one matching line, so the padding is on that line.
    format!(
        "fn {name}() {{ let _ = \"{marker}\"; {} }}\n",
        "x".repeat(width)
    )
}

#[test]
fn eight_windows_report_context_overflow_bytes_without_raising_the_cap() {
    let question = "where is budget marker";
    let under = context_window("under_marker", "budget marker", 1500);
    let over = context_window("over_marker", "budget marker \"quoted\"", 2821);
    let files = |prefix: &str, source: &str| -> Vec<(String, String)> {
        (0..8)
            .map(|index| (format!("{prefix}{index}.rs"), source.to_owned()))
            .collect()
    };
    let within = files("within", &under);
    let mut one_past = files("beyond", &over);
    let mut line = one_past[0].1.trim_end_matches('\n').to_owned();
    line.push_str(&"y".repeat(400));
    line.push('\n');
    one_past[0].1 = line;
    let run = |files: &[(String, String)], adk: &str, args: &[&str]| {
        let fixture = Fixture::new();
        for (name, source) in files {
            fs::write(fixture.root.join(name), source).expect("window");
        }
        Command::new(env!("CARGO_BIN_EXE_pbi-rs"))
            .env_clear()
            .env("PBI_RS_ADK_ENABLE", adk)
            .env("CLIPROXY_BASE_URL", "http://localhost:18317/v1")
            .env("LOCAL_MODEL", "abliterated-qwen-latest-27b-none")
            .env("CLIPROXY_API_KEY", "synthetic-not-a-provider-secret")
            .current_dir(&fixture.root)
            .args(args)
            .output()
            .expect("run")
    };
    let ok = run(&within, "0", &[question, "--timeout", "30"]);
    let ok_err = String::from_utf8_lossy(&ok.stderr);
    assert!(ok.status.success(), "ordinary evidence: {ok_err}");
    assert!(!ok_err.contains("context_overflow"));
    assert!(!ok_err.contains("attempts="));
    let blocked = run(
        &one_past,
        "1",
        &["where is budget marker quoted", "--timeout", "30"],
    );
    let stderr = String::from_utf8_lossy(&blocked.stderr);
    assert_eq!(blocked.status.code(), Some(1), "{stderr}");
    assert!(
        blocked.stdout.is_empty(),
        "{}",
        String::from_utf8_lossy(&blocked.stdout)
    );
    assert!(stderr.contains("stage=answer"), "{stderr}");
    assert!(stderr.contains("context_overflow"), "{stderr}");
    assert!(stderr.contains("bytes="), "{stderr}");
    assert!(stderr.contains("limit=24576"), "{stderr}");
    assert!(!stderr.contains("attempts="), "{stderr}");
    assert!(!stderr.contains("budget marker"), "{stderr}");
    assert!(!stderr.contains("quoted"), "{stderr}");
    assert!(!stderr.contains(".rs"), "{stderr}");
}
