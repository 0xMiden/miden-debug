use alloc::string::ToString;
use std::{
    net::{TcpListener, TcpStream},
    sync::mpsc,
    thread,
    time::Duration,
};

use miden_assembly::Assembler;

use super::*;

#[test]
fn standalone_server_runs_with_remote_state_client() {
    let directory = tempfile::tempdir().unwrap();
    let package_path = directory.path().join("standalone.masp");
    let source_manager = Arc::new(DefaultSourceManager::default());
    let package = Assembler::new(source_manager)
        .assemble_program("standalone", "begin push.3 push.4 add add end")
        .unwrap();
    package.write_to_file(&package_path).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let timeout = Duration::from_secs(5);

    let config = Box::new(DebuggerConfig {
        input: Some(crate::InputFile::from_path(&package_path)),
        start_debug_adapter: Some(address.to_string()),
        ..Default::default()
    });
    let (completed, completion) = mpsc::channel();
    let server = thread::spawn(move || {
        completed.send(run_on_listener(config, listener)).unwrap();
    });

    let stream = TcpStream::connect_timeout(&address, timeout).unwrap();
    stream.set_read_timeout(Some(timeout)).unwrap();
    stream.set_write_timeout(Some(timeout)).unwrap();
    let client = crate::exec::DapClient::from_stream(stream).unwrap();
    let mut state = crate::State::from_dap_client(&address.to_string(), client).unwrap();
    assert_eq!(state.debug_mode, crate::DebugMode::Remote);
    assert_eq!(state.executor().cycle, 0);
    assert!(matches!(state.step_remote().unwrap(), crate::exec::DapStopReason::Terminated));
    drop(state);
    completion.recv_timeout(timeout).expect("DAP server did not finish").unwrap();
    server.join().unwrap();
}
