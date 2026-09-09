//! Non-prompting checks executed in the actual supervised Portal process.
//! A Terminal-launched helper can have a different TCC attribution chain.
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    fn AXIsProcessTrusted() -> bool;
    fn CGPreflightScreenCaptureAccess() -> bool;
    fn CGPreflightListenEventAccess() -> bool;
}

pub fn status() -> serde_json::Value {
    // These preflight APIs do not request access, change grants or show prompts.
    let permissions = unsafe {
        serde_json::json!({
            "screen_recording": CGPreflightScreenCaptureAccess(),
            "accessibility": AXIsProcessTrusted(),
            "input_monitoring": CGPreflightListenEventAccess(),
        })
    };
    let status = serde_json::json!({
        "pid": std::process::id(),
        "version": crate::upgrade::PORTAL_VERSION,
        "executable": std::env::current_exe().ok(),
        "permissions": permissions,
        "scope": "This Portal process only; kit executables may have their own TCC identity.",
    });
    serde_json::json!({"content": [{"type": "text", "text": status.to_string()}], "status": status})
}
