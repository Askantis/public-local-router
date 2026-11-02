//! `cargo run -p localrouter-identity --example whois -- <pid>…`: how Local
//! Router would classify these processes (diagnostics only).
fn main() {
    for arg in std::env::args().skip(1) {
        let pid: i32 = arg.parse().expect("a process id");
        let id = localrouter_identity::identify_pid(pid);
        println!("{pid}: {:?} {:?} team={:?} key={} reason={:?}", id.trust, id.identifier, id.team, id.approval_key(), id.reason);
    }
}
