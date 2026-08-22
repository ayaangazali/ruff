use ruff_macros::{ViolationMetadata, derive_message_formats};
use ruff_python_ast::token::{TokenKind, parenthesized_range};
use ruff_python_ast::{self as ast, Expr, Operator, Stmt};
use ruff_python_trivia::is_python_whitespace;
use ruff_source_file::LineRanges;
use ruff_text_size::{Ranged, TextLen, TextRange, TextSize};

use crate::checkers::ast::Checker;
use crate::codes::Category;
use crate::{Applicability, Edit, Fix, FixAvailability, Violation};

/// ## What it does
/// Checks for string literals that are explicitly concatenated (using the
/// `+` operator).
///
/// ## Why is this bad?
/// For string literals that wrap across multiple lines, implicit string
/// concatenation within parentheses is preferred over explicit
/// concatenation using the `+` operator, as the former is more readable.
///
/// ## Example
/// ```python
/// z = (
///     "The quick brown fox jumps over the lazy "
///     + "dog"
/// )
/// ```
///
/// Use instead:
/// ```python
/// z = (
///     "The quick brown fox jumps over the lazy "
///     "dog"
/// )
/// ```
///
/// ## Options
///
/// Setting `lint.flake8-implicit-str-concat.allow-multiline = false` will disable this rule because
/// it would leave no allowed way to write a multi-line string.
///
/// - `lint.flake8-implicit-str-concat.allow-multiline`
#[derive(ViolationMetadata)]
#[violation_metadata(stable_since = "v0.0.201", category = Category::Restriction)]
pub(crate) struct ExplicitStringConcatenation;

impl Violation for ExplicitStringConcatenation {
    const FIX_AVAILABILITY: FixAvailability = FixAvailability::Sometimes;

    #[derive_message_formats]
    fn message(&self) -> String {
        "Explicitly concatenated string should be implicitly concatenated".to_string()
    }

    fn fix_title(&self) -> Option<String> {
        Some("Remove redundant '+' operator to implicitly concatenate".to_string())
    }
}

/// ISC003
pub(crate) fn explicit(checker: &Checker, expr: &Expr) {
    // If the user sets `allow-multiline` to `false`, then we should allow explicitly concatenated
    // strings that span multiple lines even if this rule is enabled. Otherwise, there's no way
    // for the user to write multiline strings, and that setting is "more explicit" than this rule
    // being enabled.
    if !checker
        .settings()
        .flake8_implicit_str_concat
        .allow_multiline
    {
        return;
    }

    if let Expr::BinOp(bin_op) = expr {
        if let ast::ExprBinOp {
            left,
            right,
            op: Operator::Add,
            ..
        } = bin_op
        {
            let concatable = matches!(
                (left.as_ref(), right.as_ref()),
                (
                    Expr::StringLiteral(_) | Expr::FString(_),
                    Expr::StringLiteral(_) | Expr::FString(_)
                ) | (Expr::BytesLiteral(_), Expr::BytesLiteral(_))
                    | (Expr::TString(_), Expr::TString(_))
            );
            if concatable
                && checker
                    .locator()
                    .contains_line_break(TextRange::new(left.end(), right.start()))
            {
                let mut diagnostic =
                    checker.report_diagnostic(ExplicitStringConcatenation, expr.range());

                let is_parenthesized = |expr: &Expr| {
                    parenthesized_range(expr.into(), bin_op.into(), checker.tokens()).is_some()
                };
                // If either `left` or `right` is parenthesized, generating
                // a fix would be too involved. Just report the diagnostic.
                // Currently, attempting `generate_fix` would result in
                // an invalid code. See: #19757
                if is_parenthesized(left) || is_parenthesized(right) {
                    return;
                }

                if let Some(edit) = generate_edit(checker, bin_op) {
                    // Dropping the `+` leaves a bare string literal behind. In docstring
                    // position that turns a statement Python ignores into the module, class,
                    // or function docstring, so `__doc__` changes at runtime:
                    //
                    // ```python
                    // (
                    //     "docstring"
                    //     + "?"
                    // )
                    // ```
                    //
                    // is `__doc__ = None` before the fix and `__doc__ = "docstring?"` after.
                    let applicability = if is_in_docstring_position(checker, expr) {
                        Applicability::Unsafe
                    } else {
                        Applicability::Safe
                    };
                    diagnostic.set_fix(Fix::applicable_edit(edit, applicability));
                }
            }
        }
    }
}

/// Returns `true` if `expr` is the whole of the first statement in a module, class, or function
/// body, which is where Python looks for a docstring.
fn is_in_docstring_position(checker: &Checker, expr: &Expr) -> bool {
    let semantic = checker.semantic();

    let statement = semantic.current_statement();
    let Stmt::Expr(ast::StmtExpr { value, .. }) = statement else {
        return false;
    };
    if value.range() != expr.range() {
        return false;
    }

    let body = match semantic.current_statement_parent() {
        Some(Stmt::FunctionDef(function_def)) => &*function_def.body,
        Some(Stmt::ClassDef(class_def)) => &*class_def.body,
        Some(_) => return false,
        None => checker.module.python_ast,
    };

    body.first()
        .is_some_and(|first| first.range() == statement.range())
}

fn generate_edit(checker: &Checker, expr_bin_op: &ast::ExprBinOp) -> Option<Edit> {
    let ast::ExprBinOp { left, right, .. } = expr_bin_op;

    let between_operands_range = TextRange::new(left.end(), right.start());
    let plus_token = checker
        .tokens()
        .in_range(between_operands_range)
        .iter()
        .find(|token| token.kind() == TokenKind::Plus)?;

    let before_plus = checker
        .locator()
        .slice(TextRange::new(left.end(), plus_token.start()));
    let after_plus = checker
        .locator()
        .slice(TextRange::new(plus_token.end(), right.start()));

    let linebreak_before_operator =
        before_plus.contains_line_break(TextRange::at(TextSize::new(0), before_plus.text_len()));

    // If removing `+` from first line trim trailing spaces
    // Preserve indentation when removing `+` from second line
    let before_plus = if linebreak_before_operator {
        before_plus
    } else {
        before_plus.trim_end_matches(is_python_whitespace)
    };

    Some(Edit::range_replacement(
        format!("{before_plus}{after_plus}"),
        between_operands_range,
    ))
}
