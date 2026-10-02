use std::os::unix::net::UnixStream;
use std::thread;

use agentos_core::guest::{Frame, Message, Mode, RAW_FRAME_LIMIT, read_frame, write_frame};

#[test]
fn frames_cross_a_real_pipe_in_both_directions() {
    let (mut a, mut b) = UnixStream::pair().unwrap();
    let hello = Message::Hello {
        protocol: 1,
        attempt_token: "0123456789abcdef0123456789abcdef".into(),
        task_id: "task".into(),
        effect_id: "effect".into(),
        attempt_id: "attempt".into(),
        lease_generation: 3,
        mode: Mode::Job,
    };
    let raw: Vec<u8> = (0..3 << 20).map(|i| (i % 251) as u8).collect();

    let (h2, r2) = (hello.clone(), raw.clone());
    let writer = thread::spawn(move || {
        write_frame(&mut a, &Frame::Json(h2)).unwrap();
        write_frame(&mut a, &Frame::Raw(r2)).unwrap();
        write_frame(&mut a, &Frame::Json(Message::Shutdown)).unwrap();
        // The reverse direction: read the reply on the same socket.
        read_frame(&mut a, RAW_FRAME_LIMIT).unwrap()
    });

    assert_eq!(read_frame(&mut b, RAW_FRAME_LIMIT).unwrap(), Frame::Json(hello));
    assert_eq!(read_frame(&mut b, RAW_FRAME_LIMIT).unwrap(), Frame::Raw(raw));
    assert_eq!(read_frame(&mut b, RAW_FRAME_LIMIT).unwrap(), Frame::Json(Message::Shutdown));
    write_frame(&mut b, &Frame::Json(Message::Bye)).unwrap();
    assert_eq!(writer.join().unwrap(), Frame::Json(Message::Bye));
}
