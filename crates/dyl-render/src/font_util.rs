//! 字体数据工具 — 字幕与弹幕引擎共用的纯字节操作

/// TTC face 选择时优先匹配的 family 关键字（按优先级排列）
///
/// 系统 CJK TTC 的 face #0 通常是 JP（如 Android 的 `NotoSansCJK-Regular.ttc`：
/// JP/KR/SC/TC/HK + 对应 Mono），直接取 #0 会让中文用日文字形渲染。
pub const CJK_SC_PREFER: &[&str] = &[
    "CJK SC",
    "Sans SC",
    "HanSans SC",
    "Simplified Chinese",
    "简体中文",
];

/// [`extract_ttc_preferred`] 选出的 face
pub struct TtcFace {
    /// 在原 TTC 中的 face 序号
    pub index: usize,
    /// name 表里的 family 名（解析失败时为空字符串）
    pub family: String,
    /// 已重写 offset table 的独立字体数据
    pub data: Vec<u8>,
}

/// 按 family 关键字从 TTC 中挑一个 face 提取为独立字体
///
/// 非 TTC 输入、或选中的 face 提取失败时返回 `None`，调用方按原样使用字节即可
/// （引擎侧对 face #0 的旧行为作为兜底仍然有效）。
pub fn extract_ttc_preferred(data: &[u8], prefer: &[&str]) -> Option<TtcFace> {
    let offsets = ttc_face_offsets(data)?;
    let families: Vec<String> = offsets
        .iter()
        .map(|&o| face_family(data, o).unwrap_or_default())
        .collect();

    // 等宽 face 不适合做正文字幕/弹幕，有比例字体时先排除
    let mut pool: Vec<usize> = (0..offsets.len())
        .filter(|&i| !families[i].to_lowercase().contains("mono"))
        .collect();
    if pool.is_empty() {
        pool = (0..offsets.len()).collect();
    }

    let mut chosen = pool[0];
    'keyword: for keyword in prefer {
        let needle = keyword.to_lowercase();
        for &i in &pool {
            if families[i].to_lowercase().contains(&needle) {
                chosen = i;
                break 'keyword;
            }
        }
    }

    let extracted = extract_font_from_ttc(data, offsets[chosen])?;
    Some(TtcFace {
        index: chosen,
        family: families[chosen].clone(),
        data: extracted,
    })
}

/// 按**序号**从 TTC 中取出指定 face，重建为独立字体数据
///
/// [`extract_ttc_preferred`] 靠 family 关键字猜 face，而字幕样式给的是确切的
/// `Fontname`，此时必须取字体索引自己声明的那一个 face，不能猜。
/// 非 TTC 输入或序号越界时返回 `None`。
pub fn extract_ttc_face(data: &[u8], index: usize) -> Option<TtcFace> {
    let offsets = ttc_face_offsets(data)?;
    let font_offset = *offsets.get(index)?;
    let extracted = extract_font_from_ttc(data, font_offset)?;
    Some(TtcFace {
        index,
        family: face_family(data, font_offset).unwrap_or_default(),
        data: extracted,
    })
}

/// 读取 TTC 头，返回每个 face 的 offset table 绝对偏移
fn ttc_face_offsets(data: &[u8]) -> Option<Vec<usize>> {
    if data.len() < 16 || &data[0..4] != b"ttcf" {
        return None;
    }
    let num_fonts = u32::from_be_bytes(data[8..12].try_into().ok()?) as usize;
    if num_fonts == 0 || data.len() < 12 + num_fonts * 4 {
        return None;
    }
    let mut offsets = Vec::with_capacity(num_fonts);
    for i in 0..num_fonts {
        let rec = 12 + i * 4;
        let offset = u32::from_be_bytes(data[rec..rec + 4].try_into().ok()?) as usize;
        if offset + 12 > data.len() {
            return None;
        }
        offsets.push(offset);
    }
    Some(offsets)
}

/// 读取某个 face 的 family 名（nameID 16 优先于 1）
fn face_family(data: &[u8], font_offset: usize) -> Option<String> {
    let num_tables = u16::from_be_bytes(
        data.get(font_offset + 4..font_offset + 6)?
            .try_into()
            .ok()?,
    ) as usize;
    if data.len() < font_offset + 12 + num_tables * 16 {
        return None;
    }
    let mut name_table = None;
    for i in 0..num_tables {
        let rec = font_offset + 12 + i * 16;
        if data[rec..rec + 4] == *b"name" {
            name_table =
                Some(u32::from_be_bytes(data[rec + 8..rec + 12].try_into().ok()?) as usize);
            break;
        }
    }
    let ntab = name_table?;
    let count = u16::from_be_bytes(data.get(ntab + 2..ntab + 4)?.try_into().ok()?) as usize;
    let storage =
        ntab + u16::from_be_bytes(data.get(ntab + 4..ntab + 6)?.try_into().ok()?) as usize;

    let mut family = None;
    let mut typographic = None;
    for i in 0..count {
        let rec = ntab + 6 + i * 12;
        let Some(entry) = data.get(rec..rec + 12) else {
            break;
        };
        let name_id = u16::from_be_bytes(entry[6..8].try_into().ok()?);
        if name_id != 1 && name_id != 16 {
            continue;
        }
        let platform = u16::from_be_bytes(entry[0..2].try_into().ok()?);
        let len = u16::from_be_bytes(entry[8..10].try_into().ok()?) as usize;
        let off = u16::from_be_bytes(entry[10..12].try_into().ok()?) as usize;
        let Some(raw) = data.get(storage + off..storage + off + len) else {
            continue;
        };
        let text = if platform == 3 || platform == 0 {
            let units: Vec<u16> = raw
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u16::from_be_bytes(*c))
                .collect();
            String::from_utf16_lossy(&units)
        } else {
            String::from_utf8_lossy(raw).into_owned()
        };
        if name_id == 16 {
            typographic = Some(text);
        } else {
            family = Some(text);
        }
    }
    typographic.or(family)
}

/// 从 TTC (TrueType Collection) 文件中提取单个字体为独立的 TTF 数据
///
/// TTC 文件中每个字体的 offset table 使用绝对文件偏移，
/// 直接切片后 `FontRef::try_from_slice` 无法正确解析。
/// 此函数重建一个自包含的 sfnt：offset table 放在缓冲区开头，
/// 各表数据按 4 字节对齐紧随其后，offset 全部按新布局重写。
///
/// 不能简单地"切片 + 减去 base"：TTC 里多个 face 共享的表可能排在后序
/// face 的 offset table **之前**（Android/Linux 的 `NotoSansCJK-Regular.ttc`
/// 就是这样），那样切片结果的开头是一段表数据而非 offset table。
pub fn extract_font_from_ttc(data: &[u8], font_offset: usize) -> Option<Vec<u8>> {
    if data.len() < font_offset + 12 {
        return None;
    }

    // 读取 offset table: sfVersion(4) + numTables(2) + searchRange(2) + entrySelector(2) + rangeShift(2)
    let num_tables =
        u16::from_be_bytes(data[font_offset + 4..font_offset + 6].try_into().ok()?) as usize;
    let header_size = 12 + num_tables * 16; // offset table header + table records
    if data.len() < font_offset + header_size {
        return None;
    }

    // 收集各表在原文件中的位置，顺便校验越界
    let mut slots = Vec::with_capacity(num_tables);
    for i in 0..num_tables {
        let rec = font_offset + 12 + i * 16;
        let offset = u32::from_be_bytes(data[rec + 8..rec + 12].try_into().ok()?) as usize;
        let length = u32::from_be_bytes(data[rec + 12..rec + 16].try_into().ok()?) as usize;
        if offset + length > data.len() {
            return None;
        }
        slots.push((offset, length));
    }

    // offset table 原样复制（含 searchRange 等字段），再逐表重写 offset 并追加表数据
    let mut result = data[font_offset..font_offset + header_size].to_vec();
    for (i, &(abs_offset, length)) in slots.iter().enumerate() {
        let new_offset = result.len() as u32;
        let rec = 12 + i * 16;
        result[rec + 8..rec + 12].copy_from_slice(&new_offset.to_be_bytes());
        result.extend_from_slice(&data[abs_offset..abs_offset + length]);
        let pad = (4 - length % 4) % 4;
        result.resize(result.len() + pad, 0);
    }

    Some(result)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 构造只含 head + name 表的合成 TTC，每个 face 的 family 为 `families[i]`
    fn synthetic_ttc(families: &[&str]) -> Vec<u8> {
        let num = families.len();
        let header_len = 12 + num * 4;
        let face_table_len = 12 + 2 * 16;
        let head_len = 8usize;

        let mut face_offsets = Vec::with_capacity(num);
        let mut cursor = header_len;
        for _ in 0..num {
            face_offsets.push(cursor);
            cursor += face_table_len + head_len;
            let name_len = 6 + 12 + families[face_offsets.len() - 1].len() * 2;
            cursor += (name_len + 3) & !3;
        }

        let mut out = Vec::new();
        out.extend_from_slice(b"ttcf");
        out.extend_from_slice(&[0, 1, 0, 0]);
        out.extend_from_slice(&(num as u32).to_be_bytes());
        for off in &face_offsets {
            out.extend_from_slice(&(*off as u32).to_be_bytes());
        }
        assert_eq!(out.len(), header_len);

        for family in families {
            let utf16: Vec<u8> = family
                .encode_utf16()
                .flat_map(|u| u.to_be_bytes())
                .collect();
            let face_off = out.len();
            let head_off = face_off + face_table_len;
            let name_off = head_off + head_len;

            out.extend_from_slice(&[0, 1, 0, 0]); // sfVersion
            out.extend_from_slice(&2u16.to_be_bytes()); // numTables: head + name
            out.extend_from_slice(&[0; 6]); // searchRange / entrySelector / rangeShift
            for (tag, off, len) in [
                (*b"head", head_off, head_len),
                (*b"name", name_off, 18 + utf16.len()),
            ] {
                out.extend_from_slice(&tag);
                out.extend_from_slice(&0u32.to_be_bytes()); // checksum
                out.extend_from_slice(&(off as u32).to_be_bytes());
                out.extend_from_slice(&(len as u32).to_be_bytes());
            }
            out.resize(head_off + head_len, 0);
            out.extend_from_slice(&0u16.to_be_bytes()); // name format
            out.extend_from_slice(&1u16.to_be_bytes()); // count
            out.extend_from_slice(&18u16.to_be_bytes()); // stringOffset
            out.extend_from_slice(&3u16.to_be_bytes()); // platformID: Windows
            out.extend_from_slice(&1u16.to_be_bytes()); // encodingID: BMP
            out.extend_from_slice(&0x0409u16.to_be_bytes()); // language
            out.extend_from_slice(&1u16.to_be_bytes()); // nameID: family
            out.extend_from_slice(&(utf16.len() as u16).to_be_bytes());
            out.extend_from_slice(&0u16.to_be_bytes()); // offset into storage
            out.extend_from_slice(&utf16);
            let pad = (4 - out.len() % 4) % 4;
            out.resize(out.len() + pad, 0);
        }
        out
    }

    #[test]
    fn test_extract_ttc_preferred_picks_sc_over_jp() {
        let ttc = synthetic_ttc(&["Noto Sans CJK JP", "Noto Sans CJK SC"]);
        let face = extract_ttc_preferred(&ttc, CJK_SC_PREFER).expect("should pick a face");
        assert_eq!(face.index, 1);
        assert_eq!(face.family, "Noto Sans CJK SC");
        assert!(!face.data.is_empty());
    }

    #[test]
    fn test_extract_ttc_preferred_skips_mono_face() {
        let ttc = synthetic_ttc(&["Noto Sans Mono CJK SC", "Noto Sans CJK JP"]);
        let face = extract_ttc_preferred(&ttc, CJK_SC_PREFER).expect("should pick a face");
        assert_eq!(face.index, 1, "等宽 face 不应因为命中 SC 就被选中");
    }

    #[test]
    fn test_extract_ttc_preferred_falls_back_to_first_face() {
        let ttc = synthetic_ttc(&["Noto Sans CJK JP", "Noto Sans CJK KR"]);
        let face = extract_ttc_preferred(&ttc, CJK_SC_PREFER).expect("should pick a face");
        assert_eq!(face.index, 0);
    }

    #[test]
    fn test_extract_ttc_preferred_rejects_non_ttc() {
        assert!(extract_ttc_preferred(b"\x00\x01\x00\x00nothing", CJK_SC_PREFER).is_none());
        // 头声明了 face 但数据被截断
        let mut truncated = b"ttcf".to_vec();
        truncated.extend_from_slice(&[0, 1, 0, 0]);
        truncated.extend_from_slice(&1u32.to_be_bytes());
        truncated.extend_from_slice(&9999u32.to_be_bytes());
        assert!(extract_ttc_preferred(&truncated, CJK_SC_PREFER).is_none());
    }

    #[test]
    fn test_extract_ttc_face_uses_the_requested_index() {
        let ttc = synthetic_ttc(&["Face One", "Face Two", "Face Three"]);
        for (index, want) in [(0, "Face One"), (1, "Face Two"), (2, "Face Three")] {
            let face = extract_ttc_face(&ttc, index).expect("face at index");
            assert_eq!(face.index, index);
            assert_eq!(face.family, want, "必须取序号指定的 face，而不是猜出来的");
            assert!(!face.data.is_empty());
        }
        assert!(extract_ttc_face(&ttc, 3).is_none(), "序号越界不得 panic");
        assert!(extract_ttc_face(b"\x00\x01\x00\x00nothing", 0).is_none());
    }

    #[test]
    fn test_extract_font_from_ttc_synthetic() {
        // 构建最小合成 TTC 文件：
        // - TTC header: "ttcf" + version + numFonts=1 + offset to font 0
        // - Font offset table: sfVersion + numTables=1 + header fields
        // - 1 个 table record: tag + checksum + offset + length
        // - table 数据

        let mut ttc = Vec::new();

        // TTC header (12 + 4 = 16 bytes)
        ttc.extend_from_slice(b"ttcf"); // tag
        ttc.extend_from_slice(&[0, 1, 0, 0]); // version 1.0
        ttc.extend_from_slice(&[0, 0, 0, 1]); // numFonts = 1
        ttc.extend_from_slice(&[0, 0, 0, 16]); // offset[0] = 16 (font starts right after TTC header)

        // Font offset table at byte 16 (12 bytes header + 16 bytes per table)
        let font_start = 16usize;
        let num_tables = 1u16;
        ttc.extend_from_slice(&[0, 1, 0, 0]); // sfVersion (TrueType)
        ttc.extend_from_slice(&num_tables.to_be_bytes()); // numTables
        ttc.extend_from_slice(&[0, 0]); // searchRange
        ttc.extend_from_slice(&[0, 0]); // entrySelector
        ttc.extend_from_slice(&[0, 0]); // rangeShift

        // Table record: "head" + checksum + offset + length
        let table_data_offset = font_start + 12 + 16; // 12 (offset table) + 16 (1 record)
        let table_data_len = 8u32;
        ttc.extend_from_slice(b"head"); // tag
        ttc.extend_from_slice(&[0, 0, 0, 0]); // checksum
        ttc.extend_from_slice(&(table_data_offset as u32).to_be_bytes()); // absolute offset
        ttc.extend_from_slice(&table_data_len.to_be_bytes()); // length

        // Table data
        ttc.extend_from_slice(&[0u8; 8]);

        // 执行提取
        let extracted = extract_font_from_ttc(&ttc, font_start).expect("extraction should succeed");

        // 验证：提取后的数据应从 table_data_offset 开始
        // offset table 中的表偏移应被重写为相对于 base 的值
        // base = font_start = 16
        // 原始绝对偏移 = table_data_offset = 44
        // 重写后 = 44 - 16 = 28
        let rec_start = 12; // offset table 内 table record 起始
        let rewritten_offset =
            u32::from_be_bytes(extracted[rec_start + 8..rec_start + 12].try_into().unwrap());
        assert_eq!(rewritten_offset, (table_data_offset - font_start) as u32);

        assert!(!extracted.is_empty());
    }

    #[test]
    fn test_extract_font_from_ttc_invalid() {
        // 太短的数据
        assert!(extract_font_from_ttc(&[0u8; 5], 0).is_none());
        // 偏移超出范围
        assert!(extract_font_from_ttc(&[0u8; 20], 100).is_none());
    }

    /// 把提取结果当作独立 sfnt 校验：头在 0 字节、每个表都落在缓冲区里且内容一致
    fn assert_valid_sfnt(extracted: &[u8], source: &[u8]) {
        assert_eq!(
            &extracted[0..4],
            b"\x00\x01\x00\x00",
            "独立字体必须以 offset table 开头"
        );
        let num_tables = u16::from_be_bytes(extracted[4..6].try_into().unwrap()) as usize;
        let header_size = 12 + num_tables * 16;
        assert!(extracted.len() >= header_size);
        for i in 0..num_tables {
            let rec = 12 + i * 16;
            let tag = &extracted[rec..rec + 4];
            let off = u32::from_be_bytes(extracted[rec + 8..rec + 12].try_into().unwrap()) as usize;
            let len =
                u32::from_be_bytes(extracted[rec + 12..rec + 16].try_into().unwrap()) as usize;
            assert!(
                off + len <= extracted.len(),
                "表 {} 越界",
                String::from_utf8_lossy(tag)
            );
            let body = &extracted[off..off + len];
            let abs = source
                .windows(len)
                .position(|w| w == body)
                .expect("表数据应能在原文件中找到");
            assert_eq!(&extracted[off..off + len], &source[abs..abs + len]);
        }
    }

    #[test]
    fn test_extract_font_from_ttc_tables_before_later_face_header() {
        // 复刻 Android/Linux NotoSansCJK TTC 的布局：多 face 共享的表排在后序 face
        // 的 offset table 之前。旧实现按 [min(table), max(table)] 切片，会把这段
        // 共享表数据留在缓冲区开头，导致 FontRef 报 InvalidFont。
        let mut ttc = Vec::new();
        ttc.extend_from_slice(b"ttcf");
        ttc.extend_from_slice(&[0, 1, 0, 0]);
        ttc.extend_from_slice(&2u32.to_be_bytes()); // numFonts = 2
        ttc.extend_from_slice(&0u32.to_be_bytes()); // offset[0]（本测试不用）
        ttc.extend_from_slice(&0u32.to_be_bytes()); // offset[1]（下面回填）

        let shared_off = ttc.len(); // 20，早于 face 1 的 offset table
        let shared_len = 12usize;
        ttc.extend_from_slice(&[7u8; 12]);

        let face_off = ttc.len(); // 32
        ttc.resize(face_off + 12 + 2 * 16, 0);
        let own_off = ttc.len();
        let own_len = 8usize;
        ttc.extend_from_slice(&vec![3u8; own_len]);

        // TTC 头的 dirList 在 12 起，offset[1] 位于 16..20
        ttc[16..20].copy_from_slice(&(face_off as u32).to_be_bytes());

        ttc[face_off..face_off + 4].copy_from_slice(b"\x00\x01\x00\x00");
        ttc[face_off + 4..face_off + 6].copy_from_slice(&2u16.to_be_bytes());
        for (i, (tag, off, len)) in [
            (*b"glyf", shared_off, shared_len),
            (*b"cvt ", own_off, own_len),
        ]
        .iter()
        .enumerate()
        {
            let rec = face_off + 12 + i * 16;
            ttc[rec..rec + 4].copy_from_slice(tag);
            ttc[rec + 8..rec + 12].copy_from_slice(&(*off as u32).to_be_bytes());
            ttc[rec + 12..rec + 16].copy_from_slice(&(*len as u32).to_be_bytes());
        }

        let extracted = extract_font_from_ttc(&ttc, face_off).expect("extraction should succeed");
        // 头(44) + 两张表(12 + 8)，不再包含 face offset table 之前的原始字节
        assert_eq!(extracted.len(), 44 + 12 + 8);
        assert_valid_sfnt(&extracted, &ttc);
    }

    #[test]
    fn test_extract_font_from_ttc_output_is_self_contained() {
        let ttc = synthetic_ttc(&["Noto Sans CJK JP", "Noto Sans CJK SC"]);
        let offsets = ttc_face_offsets(&ttc).expect("ttc header should parse");
        for off in offsets {
            let extracted = extract_font_from_ttc(&ttc, off).expect("extraction should succeed");
            assert_valid_sfnt(&extracted, &ttc);
        }
    }
}
