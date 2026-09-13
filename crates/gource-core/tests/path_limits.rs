use gource_core::{PathError, RepositoryPath};

const MAX_PATH_BYTES: usize = 64 * 1024;
const MAX_PATH_COMPONENTS: usize = 256;

fn components(count: usize) -> String {
    std::iter::repeat_n("component", count)
        .collect::<Vec<_>>()
        .join("/")
}

fn assert_path_error(input: &str, max_bytes: usize, max_components: usize, expected: PathError) {
    let result = RepositoryPath::parse_with_limits(input, max_bytes, max_components);
    assert_eq!(result, Err(expected));
}

#[test]
fn zero_component_limit_rejects_a_nonempty_path_without_publishing_one() {
    assert_path_error(
        "component",
        MAX_PATH_BYTES,
        0,
        PathError::TooDeep { limit: 0 },
    );
}

#[test]
fn exactly_256_components_remains_a_valid_path() {
    let input = components(MAX_PATH_COMPONENTS);
    let path = RepositoryPath::parse_with_limits(&input, MAX_PATH_BYTES, MAX_PATH_COMPONENTS)
        .expect("the component limit is inclusive");

    assert_eq!(path.component_count(), MAX_PATH_COMPONENTS);
    assert_eq!(path.components().len(), MAX_PATH_COMPONENTS);
    assert_eq!(path.canonical(), input);
    assert!(path.is_file());
}

#[test]
fn components_257_return_a_typed_error_without_publishing_a_partial_path() {
    let input = components(MAX_PATH_COMPONENTS + 1);

    assert_path_error(
        &input,
        MAX_PATH_BYTES,
        MAX_PATH_COMPONENTS,
        PathError::TooDeep {
            limit: MAX_PATH_COMPONENTS,
        },
    );
}

#[test]
fn repeated_separators_return_empty_component_errors_without_publishing_a_partial_path() {
    for input in ["a//b", "a///b", "/a//b", "a//b/"] {
        assert_path_error(
            input,
            MAX_PATH_BYTES,
            MAX_PATH_COMPONENTS,
            PathError::EmptyComponent,
        );
    }
}

#[test]
fn dot_and_dotdot_components_return_typed_traversal_errors() {
    for (input, component) in [
        (".", "."),
        ("..", ".."),
        ("a/./b", "."),
        ("a/../b", ".."),
        ("/../b", ".."),
    ] {
        assert_path_error(
            input,
            MAX_PATH_BYTES,
            MAX_PATH_COMPONENTS,
            PathError::Traversal {
                component: component.to_owned(),
            },
        );
    }
}

#[test]
fn exact_64k_many_component_input_returns_typed_limit_error_without_partial_publication() {
    let input = "x/".repeat(MAX_PATH_BYTES / 2);
    assert_eq!(input.len(), MAX_PATH_BYTES);

    assert_path_error(
        &input,
        MAX_PATH_BYTES,
        MAX_PATH_COMPONENTS,
        PathError::TooDeep {
            limit: MAX_PATH_COMPONENTS,
        },
    );
}
