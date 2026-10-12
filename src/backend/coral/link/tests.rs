use super::*;

#[test]
fn copy_u32_writes_only_its_field() {
    const CASES: &[(usize, u32)] = &[(0, 0xdead_beef), (70, 0x8000_0001), (198, 0x1234_5678), (7, u32::MAX), (96, 0)];
    for &(offset_bit, value) in CASES {
        let mut buf = vec![0xaau8; 32];
        copy_u32(&mut buf, offset_bit, value).unwrap();
        assert_eq!(read_u32(&buf, offset_bit).unwrap(), value, "offset {offset_bit}");
        for bit in 0..buf.len() * 8 {
            if (offset_bit..offset_bit + 32).contains(&bit) {
                continue;
            }
            let expected = (0xaau8 >> (bit % 8)) & 1;
            assert_eq!((buf[bit / 8] >> (bit % 8)) & 1, expected, "bit {bit} changed for offset {offset_bit}");
        }
    }
}

#[test]
fn fields_must_fit() {
    let mut buf = vec![0u8; 8];
    assert!(matches!(copy_u32(&mut buf, 33, 1), Err(Error::Link(_))));
    assert!(matches!(read_u32(&buf, usize::MAX - 4), Err(Error::Link(_))));
    copy_u32(&mut buf, 32, 1).unwrap();
}

fn bitstream() -> Bitstream {
    Bitstream {
        data: vec![0; 48],
        relocs: vec![
            FieldOffset { desc: Description::InputActivation, batch: 0, name: Some("x".into()), upper: true, offset_bit: 70 },
            FieldOffset { desc: Description::InputActivation, batch: 0, name: Some("x".into()), upper: false, offset_bit: 198 },
            FieldOffset { desc: Description::Parameter, batch: 0, name: None, upper: false, offset_bit: 326 },
        ],
    }
}

#[test]
fn link_resolves_every_relocation() {
    let mut addresses = Addresses { parameter: Some(0x1234_0000), ..Addresses::default() };
    addresses.inputs.insert("x".into(), vec![0x8000_0000_0001_f000]);
    let data = link(&bitstream(), &addresses).unwrap();
    assert_eq!(read_u32(&data, 70).unwrap(), 0x8000_0000);
    assert_eq!(read_u32(&data, 198).unwrap(), 0x0001_f000);
    assert_eq!(read_u32(&data, 326).unwrap(), 0x1234_0000);
}

#[test]
fn link_refuses_unresolved_relocations() {
    let cases: Vec<(Addresses, &str)> = vec![
        (Addresses::default(), "input \"x\""),
        ({ let mut a = Addresses::default(); a.inputs.insert("x".into(), vec![1]); a }, "the parameter address"),
        ({ let mut a = Addresses { parameter: Some(1), ..Addresses::default() }; a.inputs.insert("x".into(), Vec::new()); a }, "batch 0 of input"),
    ];
    for (addresses, expected) in cases {
        let message = link(&bitstream(), &addresses).unwrap_err().to_string();
        assert!(message.contains(expected), "expected {expected:?} in {message:?}");
    }
}
