//! Ask the running kernel where a struct's fields actually are.
//!
//! The diagnostic for "a kernel bumped and an offset constant may have moved":
//!
//! ```text
//! cargo run -p zensight-btf --example probe -- tcp_sock bytes_acked segs_out
//! cargo run -p zensight-btf --example probe -- trace_event_raw_inet_sock_set_state
//! ```
//!
//! With no members named, prints the struct's size only — enough to tell
//! "renamed" from "moved", which is the distinction that matters when a
//! tracepoint changes its event class and takes every field with it.

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(strukt) = args.next() else {
        eprintln!("usage: probe <struct> [member...]");
        std::process::exit(64);
    };
    let members: Vec<String> = args.collect();

    let Some(blob) = zensight_btf::read_vmlinux() else {
        eprintln!(
            "cannot read {} — a kernel without CONFIG_DEBUG_INFO_BTF, or not Linux",
            zensight_btf::VMLINUX_PATH
        );
        std::process::exit(1);
    };

    match zensight_btf::struct_size(&blob, &strukt) {
        Some(size) => println!("struct {strukt}: {size} bytes"),
        None => {
            println!("struct {strukt}: NOT IN THIS KERNEL'S BTF");
            std::process::exit(2);
        }
    }
    for m in &members {
        match zensight_btf::member_offset(&blob, &strukt, m) {
            Some(off) => println!("  {m:<16} @ {off}"),
            None => println!("  {m:<16} ABSENT"),
        }
    }
}
