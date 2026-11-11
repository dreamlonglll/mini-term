//! 编码识别与编解码的测试:BOM、合法 / 损坏的 UTF-8、按地区猜旧编码、逐字节写回、
//! 表示不了的字符。样本句子取自 Winc(Apache-2.0)`document/encoding/tests.rs`。

use super::*;

fn text_of(decoded: &Decoded) -> (&str, TextEncoding) {
    match decoded {
        Decoded::Text { text, encoding } => (text.as_str(), *encoding),
        Decoded::Binary => panic!("不该判成二进制"),
    }
}

/// UTF-8 一字不动:带不带 BOM 都原样返回(BOM 留在文本里),写回逐字节相同。
#[test]
fn utf8_is_returned_verbatim_with_its_bom() {
    for bytes in [
        b"abc\r\n".to_vec(),
        "中文 ok\n".as_bytes().to_vec(),
        b"\xEF\xBB\xBFwith bom\n".to_vec(),
        Vec::new(),
    ] {
        let decoded = decode_in(bytes.clone(), Some("cn"));
        let (text, encoding) = text_of(&decoded);
        assert_eq!(encoding, TextEncoding::Utf8);
        assert_eq!(text.as_bytes(), bytes.as_slice());
        assert_eq!(encoding.encode(text).unwrap(), bytes);
    }
    // 合法 UTF-8 里的 NUL 照旧当文本(接入编码识别之前就是这个口径)
    let decoded = decode_in(b"a\0b".to_vec(), None);
    assert_eq!(text_of(&decoded).1, TextEncoding::Utf8);
}

#[test]
fn utf16_with_bom_round_trips() {
    let text = "中文 ok\r\n第二行 😀\n";
    for encoding in [TextEncoding::Utf16Le, TextEncoding::Utf16Be] {
        let bytes = encoding.encode(text).unwrap();
        assert!(has_utf16_bom(&bytes));
        // 满篇 NUL 也不算二进制
        assert!(!looks_binary(&bytes));
        let decoded = decode_in(bytes.clone(), None);
        assert_eq!(text_of(&decoded), (text, encoding));
        assert_eq!(encoding.encode(text).unwrap(), bytes);
    }
    // 只有 BOM = 空的 UTF-16 文件(记事本存出来就是这样)
    let empty = decode_in(vec![0xFF, 0xFE], None);
    assert_eq!(text_of(&empty), ("", TextEncoding::Utf16Le));
    // 不完整的 UTF-16:仍按 UTF-16 解,只读
    let broken = decode_in(b"\xFF\xFEa".to_vec(), None);
    assert_eq!(text_of(&broken).1, TextEncoding::Lossy);
}

/// GBK、GB18030(含四字节字符)、Big5、Shift_JIS、windows-1252:猜得出来,写回与原字节
/// 完全一致。地区提示按各自的用户给(简体中文用户的机器上,短小的 Big5 文本会被往 GBK
/// 那边拉 —— 检测器的固有取舍,与 Winc 相同)。
#[test]
fn legacy_encodings_are_guessed_and_written_back() {
    let samples = [
        (
            "你好，世界。这是一段用来猜编码的中文，里面有足够多的汉字。\n",
            encoding_rs::GBK,
            Some("cn"),
        ),
        (
            "// 注释：四字节字符 𠀀 与 😀 只有 GB18030 写得出来。\n这是一段用来猜编码的中文，里面有足够多的汉字。\n",
            encoding_rs::GB18030,
            Some("cn"),
        ),
        (
            "這是一段繁體中文的文字，用來測試編碼的猜測是否正確。\n",
            encoding_rs::BIG5,
            Some("tw"),
        ),
        (
            "これは日本語の文章です。エンコーディングを推測します。\n",
            encoding_rs::SHIFT_JIS,
            None,
        ),
        (
            "Ceci est un texte français avec des accents : é, è, à, ç.\n",
            encoding_rs::WINDOWS_1252,
            Some("cn"),
        ),
    ];
    for (text, codec, region) in samples {
        let (bytes, _, errors) = codec.encode(text);
        assert!(!errors, "{} 编不出样本", codec.name());
        let decoded = decode_in(bytes.to_vec(), region);
        let (got, encoding) = text_of(&decoded);
        assert!(
            matches!(encoding, TextEncoding::Legacy(_)),
            "{text} 没认出来:{encoding:?}"
        );
        assert_eq!(got, text, "{}", encoding.label());
        assert_eq!(encoding.encode(got).unwrap(), bytes.as_ref(), "{text}");
    }
}

/// 猜成 GBK 的记成 GB18030;短文本靠地区提示。
#[test]
fn gbk_is_recorded_as_gb18030_and_short_text_uses_the_region() {
    // 「你好」两个字的 GBK 编码:不知道地区时难以区分,简体中文用户判为 GBK
    let bytes = b"needle \xC4\xE3\xBA\xC3\n".to_vec();
    let guessed = decode_in(bytes, Some("cn"));
    assert_eq!(
        text_of(&guessed),
        ("needle 你好\n", TextEncoding::Legacy(encoding_rs::GB18030))
    );
    assert_eq!(
        TextEncoding::Legacy(encoding_rs::GB18030).label(),
        "GB18030"
    );
    assert_eq!(
        TextEncoding::Legacy(encoding_rs::SHIFT_JIS).label(),
        "Shift_JIS"
    );
}

/// 不是 UTF-8 且开头有 NUL 的才是二进制。
#[test]
fn nul_in_non_utf8_means_binary() {
    assert_eq!(
        decode_in(vec![0x00, 0x9F, 0x92, 0x96, 0xC4], Some("cn")),
        Decoded::Binary
    );
    let mut png = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
    png.extend_from_slice(&[0xFF; 64]);
    assert_eq!(decode_in(png, None), Decoded::Binary);
}

/// 大体是 UTF-8、只坏了一两处:按替换字符只读打开,不猜成别的编码整篇乱码。
#[test]
fn mostly_utf8_with_a_few_bad_bytes_opens_read_only() {
    let mut bytes = "这一段基本都是合法的 UTF-8 中文,只有末尾坏了一个字节。"
        .as_bytes()
        .to_vec();
    bytes.push(0xFF);
    let decoded = decode_in(bytes, Some("cn"));
    let (text, encoding) = text_of(&decoded);
    assert_eq!(encoding, TextEncoding::Lossy);
    assert!(text.starts_with("这一段基本都是合法的"));
    assert!(text.ends_with('\u{fffd}'));
    assert_eq!(encoding.encode(text), Err(EncodeError::ReadOnly));
}

/// 表示不了的字符拒绝编码,报第一个字符与它的行号。
#[test]
fn unmappable_characters_are_refused_with_their_line() {
    let sjis = TextEncoding::Legacy(encoding_rs::SHIFT_JIS);
    assert_eq!(
        sjis.encode("日本語\n二行目 😀 と 们\n"),
        Err(EncodeError::Unmappable {
            ch: '😀',
            line: 2,
            encoding: "Shift_JIS",
        })
    );
    let latin = TextEncoding::Legacy(encoding_rs::WINDOWS_1252);
    let err = latin.encode("café 中").unwrap_err();
    assert!(err.to_string().contains("中"), "{err}");
    // GB18030 能表示全部 Unicode
    let gb = TextEncoding::Legacy(encoding_rs::GB18030);
    assert!(gb.encode("😀 𠀀 ẞ").is_ok());
}

#[test]
fn region_follows_the_system_language() {
    assert_eq!(region_of("zh-CN").as_deref(), Some("cn"));
    assert_eq!(region_of("zh-Hans-CN").as_deref(), Some("cn"));
    assert_eq!(region_of("zh_TW.UTF-8").as_deref(), Some("tw"));
    assert_eq!(region_of("zh-Hant").as_deref(), Some("tw"));
    assert_eq!(region_of("zh").as_deref(), Some("cn"));
    assert_eq!(region_of("ja").as_deref(), Some("jp"));
    assert_eq!(region_of("en-US").as_deref(), Some("us"));
    assert_eq!(region_of("en"), None);
}
