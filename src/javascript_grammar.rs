//! Called only by the isolated worker, never by parent projection code.
use oxc_allocator::Allocator;
use oxc_ast::ast::{RegExpLiteral, StringLiteral, TemplateLiteral};
use oxc_ast_visit::Visit;
use oxc_parser::{ParseOptions, Parser};
use oxc_span::{SourceType, Span};

#[derive(Default)]
struct Literals(Vec<Span>);
impl<'a> Visit<'a> for Literals {
    fn visit_reg_exp_literal(&mut self, node: &RegExpLiteral<'a>) {
        self.0.push(node.span);
    }
    fn visit_string_literal(&mut self, node: &StringLiteral<'a>) {
        self.0.push(node.span);
    }
    // Deliberately exclude the whole template, including interpolation. This
    // preserves the existing evidence policy without a second lexical parser.
    fn visit_template_literal(&mut self, node: &TemplateLiteral<'a>) {
        self.0.push(node.span);
    }
}

pub(crate) fn spans(source: &str, typescript: bool) -> Result<Vec<(u32, u32)>, ()> {
    let allocator = Allocator::default();
    // JS and TS have different ambiguity rules (e.g. chained comparisons).
    // Choose from parent-approved ownership, never retry under another grammar.
    // Oxc resolves script vs ESM syntax during this same parse, preserving
    // script-only identifiers while admitting import/export declarations.
    let kind = if typescript {
        SourceType::ts()
    } else {
        SourceType::unambiguous()
    };
    let parsed = Parser::new(&allocator, source, kind)
        .with_options(ParseOptions {
            allow_return_outside_function: true,
            ..ParseOptions::default()
        })
        .parse();
    if parsed.fatal_error || !parsed.diagnostics.is_empty() {
        return Err(());
    }
    let mut literals = Literals::default();
    literals.visit_program(&parsed.program);
    literals
        .0
        .extend(parsed.program.comments.iter().map(|comment| comment.span));
    literals
        .0
        .sort_unstable_by_key(|span| (span.start, std::cmp::Reverse(span.end)));
    let mut spans = Vec::new();
    let mut previous = 0;
    for span in literals.0 {
        // Comments inside a masked template are already owned by that template.
        if span.end <= previous {
            continue;
        }
        if span.start < previous {
            return Err(());
        }
        spans.push((span.start, span.end));
        previous = span.end;
    }
    Ok(spans)
}
