#[test]
fn pipe_names_are_stable_and_case_insensitive() {
    let a = super::pipe_name(std::path::Path::new(r"C:\Users\Me\.local\state\mare\peers\1.sock"));
    let b = super::pipe_name(std::path::Path::new("c:/users/me/.local/state/mare/peers/1.sock"));
    assert_eq!(a, b);
    assert!(a.starts_with(r"\\.\pipe\mare-"), "{a}");
    assert_ne!(a, super::pipe_name(std::path::Path::new(r"C:\other.sock")));
}
