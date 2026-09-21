//! Interpolation du prompt : remplace uniquement `{{ input }}`.

/// Rend le `template` en remplaçant `{{ input }}` par `input`.
///
/// Tolère les espaces variables autour du placeholder : `{{input}}`, `{{ input }}`, etc.
/// Autres placeholders et braces non-reconnus sont conservés tels quels.
/// Ne réapplique jamais la substitution sur l'input lui-même.
#[must_use]
pub fn render(template: &str, input: &str) -> String {
    let mut result = String::new();
    let mut rest = template;

    loop {
        if let Some(pos) = rest.find("{{") {
            result.push_str(&rest[..pos]);
            rest = &rest[pos + 2..];

            if let Some(end_pos) = rest.find("}}") {
                let placeholder = &rest[..end_pos];
                if placeholder.trim() == "input" {
                    result.push_str(input);
                } else {
                    result.push_str("{{");
                    result.push_str(placeholder);
                    result.push_str("}}");
                }
                rest = &rest[end_pos + 2..];
            } else {
                result.push_str("{{");
                result.push_str(rest);
                break;
            }
        } else {
            result.push_str(rest);
            break;
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_substitution() {
        assert_eq!(render("{{ input }}", "hello"), "hello");
    }

    #[test]
    fn multiple_occurrences() {
        assert_eq!(render("{{ input }} and {{ input }}", "x"), "x and x");
    }

    #[test]
    fn variable_spacing() {
        assert_eq!(render("{{input}}", "a"), "a");
        assert_eq!(render("{{  input  }}", "b"), "b");
        assert_eq!(render("{{ input}}", "c"), "c");
        assert_eq!(render("{{input }}", "d"), "d");
    }

    #[test]
    fn no_placeholder() {
        assert_eq!(render("just text", "x"), "just text");
    }

    #[test]
    fn input_contains_braces() {
        assert_eq!(render("{{ input }}", "{{ x }}"), "{{ x }}");
        // Assure que le contenu substitué n'est pas re-scanné
        assert_eq!(
            render("{{ input }}", "text with {{ input }}"),
            "text with {{ input }}"
        );
    }

    #[test]
    fn template_with_unmatched_braces() {
        assert_eq!(render("{{input", "test"), "{{input");
        assert_eq!(render("{{ input }text", "test"), "{{ input }text");
    }

    #[test]
    fn other_placeholders_preserved() {
        assert_eq!(render("{{ other }}", "x"), "{{ other }}");
        assert_eq!(render("{{args.name}}", "x"), "{{args.name}}");
    }
}
