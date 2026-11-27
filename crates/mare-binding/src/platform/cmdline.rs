//! Windows command-line quoting (the `CommandLineToArgvW` / CRT rules).

/// Quote one argument the way `CommandLineToArgvW` / the CRT parse it.
pub fn quote_windows_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.chars().any(|c| matches!(c, ' ' | '\t' | '\n' | '\r' | '"')) {
        return arg.to_string();
    }
    let mut q = String::with_capacity(arg.len() + 2);
    q.push('"');
    let mut slashes = 0;
    for ch in arg.chars() {
        match ch {
            '\\' => slashes += 1,
            '"' => {
                q.push_str(&"\\".repeat(slashes * 2 + 1));
                q.push('"');
                slashes = 0;
            }
            _ => {
                q.push_str(&"\\".repeat(slashes));
                slashes = 0;
                q.push(ch);
            }
        }
    }
    q.push_str(&"\\".repeat(slashes * 2));
    q.push('"');
    q
}

pub fn argv_to_command_line(argv: &[String]) -> String {
    argv.iter().map(|a| quote_windows_arg(a)).collect::<Vec<_>>().join(" ")
}
