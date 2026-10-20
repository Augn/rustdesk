use super::{Dispatch, Endpoint, Relay};
use base::message_proto::*;
use hbb_common::protobuf::Message as _;
use std::{
    collections::HashSet,
    fs::{self, File, OpenOptions},
    io::{Read, Seek, SeekFrom, Write},
};

fn wire(union: cliprdr::Union) -> Cliprdr {
    let message = Message {
        union: Some(message::Union::Cliprdr(Cliprdr {
            union: Some(union),
            ..Default::default()
        })),
        ..Default::default()
    };
    match Message::parse_from_bytes(&message.write_to_bytes().unwrap())
        .unwrap()
        .union
    {
        Some(message::Union::Cliprdr(message)) => message,
        other => panic!("unexpected wire message: {other:?}"),
    }
}

fn deliver(relay: &mut Relay, allowed: &HashSet<u64>, dispatch: Dispatch, peer: u64) -> Cliprdr {
    assert_eq!(dispatch.endpoint, Endpoint::Remote(peer));
    assert!(relay.validate(&dispatch, allowed, true));
    wire(dispatch.message.union.unwrap())
}

#[test]
fn relays_real_file_tree_to_two_peers_with_independent_streams() {
    let root = std::env::temp_dir().join(format!("omendesk-clipboard-{}", uuid::Uuid::new_v4()));
    let source = root.join("source");
    let targets = [(2, root.join("target-b")), (3, root.join("target-c"))];
    let names = [
        "资料",
        "资料/空目录",
        "资料/空文件.bin",
        "资料/说明.txt",
        "资料/分块数据.bin",
    ];
    fs::create_dir_all(source.join(names[1])).unwrap();
    fs::write(source.join(names[2]), []).unwrap();
    fs::write(source.join(names[3]), "跨会话文件内容\n").unwrap();
    fs::write(
        source.join(names[4]),
        (0..17 * 1024 * 1024 + 31)
            .map(|index| (index % 251) as u8)
            .collect::<Vec<_>>(),
    )
    .unwrap();

    let mut descriptors = (names.len() as u32).to_le_bytes().to_vec();
    for name in names {
        let metadata = fs::metadata(source.join(name)).unwrap();
        let mut descriptor = [0; 592];
        descriptor[..4].copy_from_slice(&0x44_u32.to_le_bytes());
        descriptor[36..40]
            .copy_from_slice(&(if metadata.is_dir() { 0x10_u32 } else { 0x80 }).to_le_bytes());
        let size = if metadata.is_dir() { 0 } else { metadata.len() };
        descriptor[64..68].copy_from_slice(&((size >> 32) as u32).to_le_bytes());
        descriptor[68..72].copy_from_slice(&(size as u32).to_le_bytes());
        for (index, unit) in name.replace('/', "\\").encode_utf16().enumerate() {
            descriptor[72 + index * 2..74 + index * 2].copy_from_slice(&unit.to_le_bytes());
        }
        descriptors.extend_from_slice(&descriptor);
    }
    let list_id = descriptors.iter().fold(0x811c_9dc5_u32, |hash, byte| {
        (hash ^ u32::from(*byte)).wrapping_mul(0x0100_0193)
    }) as i32;
    let allowed = HashSet::from([1, 2, 3]);
    let mut relay = Relay::default();
    let mut formats = CliprdrServerFormatList::default();
    for (id, name) in [(100, "FileGroupDescriptorW"), (101, "FileContents")] {
        formats.formats.push(CliprdrFormat {
            id,
            format: name.into(),
            ..Default::default()
        });
    }
    let announced = relay.handle(
        1,
        false,
        wire(cliprdr::Union::FormatList(formats)),
        &allowed,
        true,
        true,
    );
    assert!(announced.message.is_some());
    assert_eq!(announced.outbound.len(), 2);
    for dispatch in announced.outbound {
        let Endpoint::Remote(peer) = dispatch.endpoint else {
            panic!("file offer must reach a remote peer");
        };
        assert!(matches!(peer, 2 | 3));
        deliver(&mut relay, &allowed, dispatch, peer);
    }

    for (index, (peer, _)) in targets.iter().enumerate() {
        let requested = relay.handle(
            *peer,
            false,
            wire(cliprdr::Union::FormatDataRequest(
                CliprdrServerFormatDataRequest {
                    requested_format_id: 100,
                    ..Default::default()
                },
            )),
            &allowed,
            false,
            true,
        );
        assert!(requested.message.is_none());
        assert_eq!(requested.outbound.len(), usize::from(index == 0));
        for dispatch in requested.outbound {
            let request = deliver(&mut relay, &allowed, dispatch, 1);
            assert!(
                matches!(request.union, Some(cliprdr::Union::FormatDataRequest(request)) if request.requested_format_id == 100)
            );
        }
    }
    let described = relay.handle(
        1,
        false,
        wire(cliprdr::Union::FormatDataResponse(
            CliprdrServerFormatDataResponse {
                msg_flags: 1,
                format_data: descriptors.clone().into(),
                ..Default::default()
            },
        )),
        &allowed,
        false,
        true,
    );
    assert!(described.message.is_none());
    assert_eq!(described.outbound.len(), 2);
    for dispatch in described.outbound {
        let Endpoint::Remote(peer) = dispatch.endpoint else {
            panic!("expected remote descriptor response")
        };
        let response = deliver(&mut relay, &allowed, dispatch, peer);
        let Some(cliprdr::Union::FormatDataResponse(response)) = response.union else {
            panic!("expected descriptors")
        };
        assert_eq!(response.format_data.as_ref(), descriptors);
        let target = &targets.iter().find(|(id, _)| *id == peer).unwrap().1;
        for descriptor in response.format_data[4..].chunks_exact(592) {
            let name = String::from_utf16(
                &descriptor[72..]
                    .chunks_exact(2)
                    .map(|unit| u16::from_le_bytes([unit[0], unit[1]]))
                    .take_while(|unit| *unit != 0)
                    .collect::<Vec<_>>(),
            )
            .unwrap()
            .replace('\\', "/");
            let path = target.join(name);
            if u32::from_le_bytes(descriptor[36..40].try_into().unwrap()) & 0x10 != 0 {
                fs::create_dir_all(path).unwrap();
            } else {
                File::create(path).unwrap();
            }
        }
    }

    const CHUNK: usize = 16 * 1024 * 1024;
    for (list_index, name) in names.iter().enumerate().skip(2) {
        let size = fs::metadata(source.join(name)).unwrap().len();
        for offset in (0..size.max(1)).step_by(CHUNK) {
            let length = (size.saturating_sub(offset) as usize).clamp(1, CHUNK);
            let mut pending = Vec::new();
            for (peer, _) in &targets {
                let requested = relay.handle(
                    *peer,
                    false,
                    wire(cliprdr::Union::FileContentsRequest(
                        CliprdrFileContentsRequest {
                            stream_id: 7,
                            list_index: list_index as i32,
                            dw_flags: 2,
                            n_position_low: offset as i32,
                            n_position_high: (offset >> 32) as i32,
                            cb_requested: length as i32,
                            have_clip_data_id: true,
                            clip_data_id: list_id,
                            ..Default::default()
                        },
                    )),
                    &allowed,
                    false,
                    true,
                );
                assert!(requested.message.is_none());
                assert_eq!(requested.outbound.len(), 1);
                let request = deliver(
                    &mut relay,
                    &allowed,
                    requested.outbound.into_iter().next().unwrap(),
                    1,
                );
                let Some(cliprdr::Union::FileContentsRequest(request)) = request.union else {
                    panic!("expected content request")
                };
                assert_eq!(request.list_index, list_index as i32);
                assert_eq!(request.dw_flags, 2);
                assert!(request.have_clip_data_id);
                assert_eq!(request.clip_data_id, list_id);
                assert!(request.stream_id < 0);
                pending.push((*peer, request));
            }
            assert_ne!(pending[0].1.stream_id, pending[1].1.stream_id);
            for (peer, request) in pending.into_iter().rev() {
                let mut input =
                    File::open(source.join(names[request.list_index as usize])).unwrap();
                let mut data = Vec::new();
                let position = (u64::from(request.n_position_high as u32) << 32)
                    | u64::from(request.n_position_low as u32);
                input.seek(SeekFrom::Start(position)).unwrap();
                input
                    .take(request.cb_requested as u64)
                    .read_to_end(&mut data)
                    .unwrap();
                let responded = relay.handle(
                    1,
                    false,
                    wire(cliprdr::Union::FileContentsResponse(
                        CliprdrFileContentsResponse {
                            msg_flags: 1,
                            stream_id: request.stream_id,
                            requested_data: data.into(),
                            ..Default::default()
                        },
                    )),
                    &allowed,
                    false,
                    true,
                );
                assert!(responded.message.is_none());
                assert_eq!(responded.outbound.len(), 1);
                let response = deliver(
                    &mut relay,
                    &allowed,
                    responded.outbound.into_iter().next().unwrap(),
                    peer,
                );
                let Some(cliprdr::Union::FileContentsResponse(response)) = response.union else {
                    panic!("expected file data")
                };
                assert_eq!(response.msg_flags, 1);
                assert_eq!(response.stream_id, 7);
                let target = &targets.iter().find(|(id, _)| *id == peer).unwrap().1;
                let mut output = OpenOptions::new()
                    .write(true)
                    .open(target.join(name))
                    .unwrap();
                output.seek(SeekFrom::Start(offset)).unwrap();
                output.write_all(&response.requested_data).unwrap();
            }
        }
    }
    for (_, target) in targets {
        for name in names {
            let metadata = fs::metadata(source.join(name)).unwrap();
            assert_eq!(
                fs::metadata(target.join(name)).unwrap().is_dir(),
                metadata.is_dir()
            );
            if metadata.is_file() {
                let digest = |path| {
                    crate::update_hash::sha256_reader_hex(&mut File::open(path).unwrap()).unwrap()
                };
                assert_eq!(
                    digest(source.join(name)),
                    digest(target.join(name)),
                    "{name}"
                );
            }
        }
    }
    fs::remove_dir_all(root).unwrap();
}
