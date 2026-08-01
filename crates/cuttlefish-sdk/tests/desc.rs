//! The host reads guest memory at addresses this crate hands it, using a layout
//! it assumes rather than negotiates. These tests pin that layout, because a
//! mismatch is not a compile error on either side — it is the host reading
//! whatever happens to be at the wrong offset.

use cuttlefish_sdk::Desc;

#[test]
fn desc_is_two_pointer_sized_fields() {
    // The host reads exactly `2 * ptr_size` bytes at the returned address and
    // splits them down the middle. Padding or reordering here would silently
    // feed it a garbage pointer.
    assert_eq!(
        std::mem::size_of::<Desc>(),
        2 * std::mem::size_of::<usize>()
    );
    assert_eq!(std::mem::align_of::<Desc>(), std::mem::align_of::<usize>());
}

#[test]
fn desc_fields_are_in_declared_order() {
    // `#[repr(C)]` is what guarantees this; the test is here so that dropping
    // the attribute fails loudly rather than producing a host that reads `len`
    // as a pointer.
    let d = Desc {
        ptr: 0xAAAA,
        len: 0xBBBB,
    };
    let base = &d as *const Desc as usize;
    let ptr_field = std::ptr::addr_of!(d.ptr) as usize - base;
    let len_field = std::ptr::addr_of!(d.len) as usize - base;
    assert_eq!(ptr_field, 0, "ptr must come first");
    assert_eq!(
        len_field,
        std::mem::size_of::<usize>(),
        "len must directly follow ptr with no padding"
    );
}

#[test]
fn write_json_produces_a_readable_descriptor() {
    // Mirrors exactly what the host does: read the Desc, then read `len` bytes
    // at `ptr`, then parse. If this test can round-trip it, the host can.
    let value = serde_json::json!({"cmd": "done", "result": {"summary": "s"}});
    let desc_addr = cuttlefish_sdk::__write_json(&value);

    let desc = unsafe { &*(desc_addr as *const Desc) };
    let bytes = unsafe { std::slice::from_raw_parts(desc.ptr as *const u8, desc.len) };
    let parsed: serde_json::Value = serde_json::from_slice(bytes).unwrap();

    assert_eq!(parsed, value);
}

#[test]
fn alloc_returns_writable_memory_of_the_requested_size() {
    // The host calls `cf_alloc` and then writes the guest's input into it.
    let len = 64usize;
    let ptr = cuttlefish_sdk::__alloc(len);
    assert_ne!(ptr, 0, "allocation must not hand back a null pointer");

    let buf = unsafe { std::slice::from_raw_parts_mut(ptr as *mut u8, len) };
    buf.fill(0x5A);
    assert!(buf.iter().all(|&b| b == 0x5A));
}

#[test]
fn read_json_decodes_what_the_host_wrote() {
    let event = cuttlefish_abi::Event::Opened {
        handle: 3,
        len: 900,
        kind: cuttlefish_abi::MediaKind::Text,
    };
    let encoded = serde_json::to_vec(&event).unwrap();

    // Stand in for the host: allocate through the guest, copy bytes in.
    let ptr = cuttlefish_sdk::__alloc(encoded.len());
    unsafe {
        std::ptr::copy_nonoverlapping(encoded.as_ptr(), ptr as *mut u8, encoded.len());
    }

    let decoded: cuttlefish_abi::Event = unsafe { cuttlefish_sdk::__read_json(ptr, encoded.len()) };
    assert_eq!(decoded, event);
}
