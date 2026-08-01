//! How a rejection names what it would have accepted.
//!
//! One rule, applied everywhere a closed set is refused: **every canonical
//! spelling is named; aliases are not.**
//!
//! An alias is a second way to type an option that is already listed, so
//! leaving `>` out of the threshold-op message costs a caller nothing. A
//! missing *canonical* spelling hides an option entirely — and a caller who
//! trusts the error then cannot reach it from the very message refusing them.
//! That is not hypothetical: three `group_by` messages named four of five
//! values, and the one they omitted was `none`, the only way to ask for the
//! overall ungrouped figures.
//!
//! The corollary is that the list must not be hand-written beside the parser.
//! Each closed-set type owns an `ALL`, and its `accepted()` renders that
//! through [`one_of`], so the message, the parser and the schema agreement
//! test cannot disagree.

/// Render a closed set as the tail of a rejection: ``` `a`, `b` or `c` ```.
pub(crate) fn one_of(values: &[&str]) -> String {
    match values {
        [] => String::new(),
        [only] => format!("`{only}`"),
        [rest @ .., last] => {
            let head: Vec<String> = rest.iter().map(|v| format!("`{v}`")).collect();
            format!("{} or `{last}`", head.join(", "))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::one_of;

    #[test]
    fn renders_by_length() {
        assert_eq!(one_of(&[]), "");
        assert_eq!(one_of(&["name"]), "`name`");
        assert_eq!(one_of(&["name", "group"]), "`name` or `group`");
        assert_eq!(
            one_of(&["none", "name", "group", "trace", "path"]),
            "`none`, `name`, `group`, `trace` or `path`"
        );
    }
}
