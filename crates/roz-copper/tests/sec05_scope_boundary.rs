//! SEC-05 scope-boundary meta-test (D-06).
//!
//! Codifies: ONLY `from_precompiled` calls `Module::deserialize` AND
//! ONLY `from_precompiled` calls `verify_detached`. Any PR that adds
//! `Module::deserialize` elsewhere or propagates verification into
//! `from_source`/`from_source_with_host` will fail this test.
//!
//! NOT gated on `--features aot` — must run in every build so a PR
//! with aot disabled still can't sneak in an ungated native-code load.

#[test]
fn only_from_precompiled_calls_module_deserialize() {
    let src = include_str!("../src/wasm.rs");

    // Locate from_precompiled's signature and walk backwards through its
    // contiguous block of `#[...]` attributes and `///` doc comments so
    // that Module::deserialize references in the function's own doc/SAFETY
    // commentary are counted as inside the function, not outside it.
    let fn_decl_byte = src
        .find("pub fn from_precompiled(")
        .expect("from_precompiled fn exists");
    // Snap to the start of the line containing the fn declaration so the
    // walker inspects the PRECEDING line (attributes/docs), not the indent
    // of the declaration line itself.
    let fn_line_start = src[..fn_decl_byte].rfind('\n').map_or(0, |n| n + 1);
    // Walk up line-by-line as long as the preceding line (trimmed) begins
    // with `#` (attribute), `///` (doc), or `//` (inner comment).
    let fp_start = {
        let mut cut = fn_line_start;
        loop {
            let prefix = &src[..cut];
            // Find start of preceding line.
            let prev_newline = prefix.trim_end_matches('\n').rfind('\n');
            let line_start = prev_newline.map_or(0, |n| n + 1);
            let line_end = cut.saturating_sub(1); // exclude the trailing \n
            if line_end < line_start {
                break;
            }
            let line = src[line_start..line_end].trim_start();
            if line.starts_with("///") || line.starts_with("#[") || line.starts_with("//") {
                cut = line_start;
                if cut == 0 {
                    break;
                }
            } else {
                break;
            }
        }
        cut
    };

    // Find the matching closing brace of from_precompiled by scanning
    // braces after the signature.
    let body_open = fn_line_start + src[fn_line_start..].find('{').expect("from_precompiled body opens");
    let mut depth: i32 = 0;
    let mut body_end = body_open;
    for (i, ch) in src[body_open..].char_indices() {
        match ch {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    body_end = body_open + i + 1;
                    break;
                }
            }
            _ => {}
        }
    }
    let fp_range = fp_start..body_end;

    // Every `Module::deserialize` occurrence MUST be inside fp_range.
    for (idx, _) in src.match_indices("Module::deserialize") {
        assert!(
            fp_range.contains(&idx),
            "Module::deserialize at byte {idx} is OUTSIDE from_precompiled \
             (range {fp_range:?}) — SEC-05 scope violation"
        );
    }
}

#[test]
fn from_source_does_not_call_verify_detached() {
    let src = include_str!("../src/wasm.rs");

    // Scan the BODY of every `pub fn from_source*` in wasm.rs (brace-matched)
    // and assert none of them reference `verify_detached`. Slicing raw text
    // between fn signatures would also cover from_precompiled's doc comments,
    // which legitimately mention verify_detached and would be false positives.
    let mut offset = 0;
    let mut checked = 0;
    while let Some(rel) = src[offset..].find("pub fn from_source") {
        let fn_sig_byte = offset + rel;
        let body_open = fn_sig_byte + src[fn_sig_byte..].find('{').expect("from_source* body opens");
        let mut depth: i32 = 0;
        let mut body_end = body_open;
        for (i, ch) in src[body_open..].char_indices() {
            match ch {
                '{' => depth += 1,
                '}' => {
                    depth -= 1;
                    if depth == 0 {
                        body_end = body_open + i + 1;
                        break;
                    }
                }
                _ => {}
            }
        }
        let body = &src[body_open..body_end];
        assert!(
            !body.contains("verify_detached"),
            "from_source* body at byte {fn_sig_byte} references verify_detached — \
             SEC-05 scope violation (D-06: verification is only invoked by from_precompiled)"
        );
        checked += 1;
        offset = body_end;
    }
    assert!(checked >= 1, "expected at least one pub fn from_source* in wasm.rs");
}
