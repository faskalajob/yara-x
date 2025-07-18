use crate::modules::prelude::*;
use crate::modules::protos::console::*;

#[module_main]
fn main(_data: &[u8], _meta: Option<&[u8]>) -> Result<Console, ModuleError> {
    // Nothing to do, but we have to return our protobuf
    Ok(Console::new())
}

#[module_export(name = "log")]
fn log_str(ctx: &mut ScanContext, string: RuntimeString) -> bool {
    ctx.console_log(format!("{}", string.as_bstr(ctx)));
    true
}

#[module_export(name = "log")]
fn log_msg_str(
    ctx: &mut ScanContext,
    message: RuntimeString,
    string: RuntimeString,
) -> bool {
    ctx.console_log(format!(
        "{}{}",
        message.as_bstr(ctx),
        string.as_bstr(ctx)
    ));
    true
}

#[module_export(name = "log")]
fn log_bool(ctx: &mut ScanContext, b: bool) -> bool {
    ctx.console_log(format!("{b}"));
    true
}

#[module_export(name = "log")]
fn log_msg_bool(
    ctx: &mut ScanContext,
    message: RuntimeString,
    b: bool,
) -> bool {
    ctx.console_log(format!("{}{}", message.as_bstr(ctx), b));
    true
}

#[module_export(name = "log")]
fn log_int(ctx: &mut ScanContext, i: i64) -> bool {
    ctx.console_log(format!("{i}"));
    true
}

#[module_export(name = "log")]
fn log_msg_int(ctx: &mut ScanContext, message: RuntimeString, i: i64) -> bool {
    ctx.console_log(format!("{}{}", message.as_bstr(ctx), i));
    true
}

#[module_export(name = "log")]
fn log_float(ctx: &mut ScanContext, f: f64) -> bool {
    ctx.console_log(format!("{f}"));
    true
}

#[module_export(name = "log")]
fn log_msg_float(
    ctx: &mut ScanContext,
    message: RuntimeString,
    f: f64,
) -> bool {
    ctx.console_log(format!("{}{}", message.as_bstr(ctx), f));
    true
}

#[module_export(name = "hex")]
fn log_hex(ctx: &mut ScanContext, i: i64) -> bool {
    ctx.console_log(format!("0x{i:x}"));
    true
}

#[module_export(name = "hex")]
fn log_msg_hex(ctx: &mut ScanContext, message: RuntimeString, i: i64) -> bool {
    ctx.console_log(format!("{}0x{:x}", message.as_bstr(ctx), i));
    true
}

#[cfg(test)]
mod tests {

    #[test]
    fn log() {
        let rules = crate::compile(
            r#"
            import "console"
            rule test {
                condition:
                    console.log("foo") and
                    console.log("bar: ", 1) and
                    console.log("baz: ", 3.14) and
                    console.log(10) and
                    console.log(6.28) and
                    console.hex(10) and
                    console.hex("qux: ", 255)
            }
            "#,
        )
        .unwrap();

        let mut messages = vec![];

        crate::scanner::Scanner::new(&rules)
            .console_log(|message| messages.push(message))
            .scan(b"")
            .expect("scan should not fail");

        assert_eq!(
            messages,
            vec![
                "foo",
                "bar: 1",
                "baz: 3.14",
                "10",
                "6.28",
                "0xa",
                "qux: 0xff"
            ]
        );
    }
}
