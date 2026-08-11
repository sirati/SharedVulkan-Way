//! The registry's `depends=` expression language.
//!
//! # Why this exists at all
//!
//! It was not in the plan. design.md §13 step 3 and §11 both talk about
//! enabling `VK_VERSION_1_0` … `VK_VERSION_1_3` as if a `<feature>` were a leaf
//! containing its own commands. As of `vulkan-headers 1.4.350` that is no longer
//! true: the core versions have been split into `apitype="internal"`
//! sub-features chained by `depends=`, and the feature that bears the familiar
//! name contains almost nothing.
//!
//! ```xml
//! <feature apitype="internal" name="VK_BASE_VERSION_1_0"     number="1.0">…73 commands…
//! <feature apitype="internal" name="VK_COMPUTE_VERSION_1_0"  depends="VK_BASE_VERSION_1_0">…36…
//! <feature apitype="internal" name="VK_GRAPHICS_VERSION_1_0" depends="VK_COMPUTE_VERSION_1_0">…28…
//! <feature                    name="VK_VERSION_1_0"          depends="VK_GRAPHICS_VERSION_1_0">…0…
//! ```
//!
//! A resolver that reads `<feature name="VK_VERSION_1_0">` and stops gets **no**
//! commands from Vulkan 1.0 and does not notice — it just produces a small
//! protocol. That is exactly the class of silent-wrongness this project is
//! supposed to be paranoid about, and it was caught only because the profile
//! test asserted a lower bound on the command count. It is recorded here rather
//! than fixed quietly.
//!
//! # The grammar
//!
//! From the registry schema, `,` is disjunction and `+` is conjunction, with
//! `+` binding tighter, and parentheses available:
//!
//! ```text
//! expr   := term (',' term)*
//! term   := factor ('+' factor)*
//! factor := '(' expr ')' | name
//! ```
//!
//! A `name` is a feature or extension name. Version names may carry a
//! `VK_VERSION_x_y` form that is satisfied by having that feature enabled, which
//! is the same rule.

/// Evaluate a `depends=` expression against a set of enabled names.
pub fn satisfied(expr: &str, enabled: &dyn Fn(&str) -> bool) -> bool {
    let tokens = tokenize(expr);
    let mut pos = 0;
    let value = parse_expr(&tokens, &mut pos, enabled);
    // A trailing token means the expression used syntax this parser does not
    // know. Treating that as "satisfied" would silently widen the profile, so
    // it is treated as unsatisfied — the conservative direction.
    if pos != tokens.len() { false } else { value }
}

/// Every name an expression mentions, in order, ignoring the operators.
///
/// Used to find the feature chain a core version hangs off, where the operators
/// do not matter because every link is required.
pub fn names(expr: &str) -> Vec<String> {
    tokenize(expr)
        .into_iter()
        .filter_map(|t| match t {
            Token::Name(n) => Some(n),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Token {
    Name(String),
    Comma,
    Plus,
    Open,
    Close,
}

fn tokenize(s: &str) -> Vec<Token> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let flush = |cur: &mut String, out: &mut Vec<Token>| {
        if !cur.is_empty() {
            out.push(Token::Name(std::mem::take(cur)));
        }
    };
    for c in s.chars() {
        match c {
            ',' => {
                flush(&mut cur, &mut out);
                out.push(Token::Comma);
            }
            '+' => {
                flush(&mut cur, &mut out);
                out.push(Token::Plus);
            }
            '(' => {
                flush(&mut cur, &mut out);
                out.push(Token::Open);
            }
            ')' => {
                flush(&mut cur, &mut out);
                out.push(Token::Close);
            }
            c if c.is_whitespace() => flush(&mut cur, &mut out),
            c => cur.push(c),
        }
    }
    flush(&mut cur, &mut out);
    out
}

fn parse_expr(tokens: &[Token], pos: &mut usize, enabled: &dyn Fn(&str) -> bool) -> bool {
    let mut value = parse_term(tokens, pos, enabled);
    while matches!(tokens.get(*pos), Some(Token::Comma)) {
        *pos += 1;
        // Not short-circuiting: the right-hand side must still be consumed, or
        // `pos` ends up wrong and the trailing-token check misfires.
        let rhs = parse_term(tokens, pos, enabled);
        value = value || rhs;
    }
    value
}

fn parse_term(tokens: &[Token], pos: &mut usize, enabled: &dyn Fn(&str) -> bool) -> bool {
    let mut value = parse_factor(tokens, pos, enabled);
    while matches!(tokens.get(*pos), Some(Token::Plus)) {
        *pos += 1;
        let rhs = parse_factor(tokens, pos, enabled);
        value = value && rhs;
    }
    value
}

fn parse_factor(tokens: &[Token], pos: &mut usize, enabled: &dyn Fn(&str) -> bool) -> bool {
    match tokens.get(*pos) {
        Some(Token::Open) => {
            *pos += 1;
            let v = parse_expr(tokens, pos, enabled);
            if matches!(tokens.get(*pos), Some(Token::Close)) {
                *pos += 1;
            }
            v
        }
        Some(Token::Name(n)) => {
            *pos += 1;
            enabled(n)
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with<'a>(set: &'a [&'a str]) -> impl Fn(&str) -> bool + 'a {
        move |n: &str| set.contains(&n)
    }

    #[test]
    fn a_bare_name() {
        assert!(satisfied("VK_VERSION_1_1", &with(&["VK_VERSION_1_1"])));
        assert!(!satisfied("VK_VERSION_1_1", &with(&[])));
    }

    #[test]
    fn comma_is_or_and_plus_is_and() {
        let have_one = with(&["A"]);
        assert!(satisfied("A,B", &have_one));
        assert!(!satisfied("A+B", &have_one));
        let have_both = with(&["A", "B"]);
        assert!(satisfied("A+B", &have_both));
    }

    /// The shape the registry actually writes: "1.1, or the extension that was
    /// promoted into it".
    #[test]
    fn the_promotion_idiom() {
        let e = "VK_VERSION_1_1,VK_KHR_get_physical_device_properties2";
        assert!(satisfied(e, &with(&["VK_VERSION_1_1"])));
        assert!(satisfied(e, &with(&["VK_KHR_get_physical_device_properties2"])));
        assert!(!satisfied(e, &with(&["VK_KHR_surface"])));
    }

    #[test]
    fn plus_binds_tighter_than_comma() {
        // A+B,C  ==  (A and B) or C
        assert!(satisfied("A+B,C", &with(&["C"])));
        assert!(satisfied("A+B,C", &with(&["A", "B"])));
        assert!(!satisfied("A+B,C", &with(&["A"])));
    }

    #[test]
    fn parentheses_override() {
        // A+(B,C) == A and (B or C)
        assert!(satisfied("A+(B,C)", &with(&["A", "C"])));
        assert!(!satisfied("A+(B,C)", &with(&["B"])));
    }

    #[test]
    fn unparseable_is_unsatisfied_not_satisfied() {
        assert!(!satisfied("A B", &with(&["A", "B"])));
    }

    #[test]
    fn names_ignores_operators() {
        assert_eq!(names("A+(B,C)"), vec!["A", "B", "C"]);
    }

    /// The chain that made this module necessary.
    #[test]
    fn the_core_version_chain_of_1_4_350() {
        assert_eq!(names("VK_GRAPHICS_VERSION_1_0"), vec!["VK_GRAPHICS_VERSION_1_0"]);
        assert_eq!(
            names("VK_COMPUTE_VERSION_1_0,VK_BASE_VERSION_1_1"),
            vec!["VK_COMPUTE_VERSION_1_0", "VK_BASE_VERSION_1_1"]
        );
    }
}
