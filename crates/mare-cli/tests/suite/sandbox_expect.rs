use mare_host::Host;

use crate::common::eval;

pub(crate) async fn require_backend(h: &Host, test: &str, accepted: &[&str]) -> bool {
    let actual: String = eval(h, "return mare.proc.sandbox.capabilities().mechanism").await;
    mare_binding::ossandbox::expect::require(test, &actual, accepted)
}
