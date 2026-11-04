//! 图形命令的控制段:`a=T,f=100,q=2,C=1,c=40,r=6,i=3525535711,m=1;<payload>` 里分号前面那截。
//!
//! 只解析原型用得上的键,其余一律跳过(`z=-1` 这种负数值也在跳过之列)。认识的键值
//! 写坏了就整条作废:转义序列是程序发的,发错了宁可不画也不要猜。

/// 控制段解析结果。没写的键是 `None`,缺省语义由使用方按规范补。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Control {
    /// `a`:动作,缺省 `t`。
    pub action: Option<u8>,
    /// `d`:删除范围,缺省 `a`。
    pub delete: Option<u8>,
    /// `f`:数据格式,缺省 32(本原型只收 100 = PNG)。
    pub format: Option<u32>,
    /// `t`:传输介质,缺省 `d`(本原型只收直传)。
    pub medium: Option<u8>,
    /// `o`:压缩。本原型不收。
    pub compression: Option<u8>,
    /// `m=1`:后面还有分块。
    pub more: bool,
    /// `i`:图片 id。0 与没写同义。
    pub image_id: Option<u32>,
    /// `I`:图片编号。本原型不收,摆放与删除都不认。
    pub image_number: Option<u32>,
    /// `p`:摆放 id。
    pub placement_id: Option<u32>,
    /// `q`:0 全答,1 不答 OK,2 全不答。
    pub quiet: u32,
    /// `c` / `r`:显示占几列几行。
    pub columns: Option<u32>,
    pub rows: Option<u32>,
    /// `x` `y` `w` `h`:源图裁切矩形(源图像素)。`w` `h` 为 0 与没写同义。
    pub src_x: u32,
    pub src_y: u32,
    pub src_w: u32,
    pub src_h: u32,
    /// `C=1`:摆放后不移动光标。原型里所有摆放都不移动(见模块注释的已知限制),这里只留档。
    pub keep_cursor: bool,
    /// `U=1`(虚拟摆放)或 `P`(相对摆放):本原型不画,只留图片数据。
    pub unsupported_placement: bool,
}

/// 把 APC 正文切成控制段与 payload。没有分号就全是控制段。
pub(super) fn split_body(body: &[u8]) -> (&[u8], &[u8]) {
    match memchr::memchr(b';', body) {
        Some(at) => (&body[..at], &body[at + 1..]),
        None => (body, &[]),
    }
}

/// 解析控制段。认识的键值写坏了返回 `None`。
pub(super) fn parse_control(raw: &[u8]) -> Option<Control> {
    let mut out = Control::default();
    for pair in raw.split(|&b| b == b',') {
        if pair.is_empty() {
            continue;
        }
        let eq = memchr::memchr(b'=', pair)?;
        let (key, value) = (&pair[..eq], &pair[eq + 1..]);
        let &[key] = key else {
            continue;
        };
        match key {
            b'a' => out.action = Some(one_byte(value)?),
            b'd' => out.delete = Some(one_byte(value)?),
            b't' => out.medium = Some(one_byte(value)?),
            b'o' => out.compression = Some(one_byte(value)?),
            b'f' => out.format = Some(number(value)?),
            b'm' => out.more = number(value)? == 1,
            b'i' => out.image_id = Some(number(value)?),
            b'I' => out.image_number = Some(number(value)?),
            b'p' => out.placement_id = Some(number(value)?),
            b'q' => out.quiet = number(value)?,
            b'c' => out.columns = Some(number(value)?),
            b'r' => out.rows = Some(number(value)?),
            b'x' => out.src_x = number(value)?,
            b'y' => out.src_y = number(value)?,
            b'w' => out.src_w = number(value)?,
            b'h' => out.src_h = number(value)?,
            b'C' => out.keep_cursor = number(value)? == 1,
            b'U' => out.unsupported_placement |= number(value)? == 1,
            b'P' => out.unsupported_placement |= number(value)? != 0,
            _ => {}
        }
    }
    Some(out)
}

fn one_byte(value: &[u8]) -> Option<u8> {
    match value {
        [byte] => Some(*byte),
        _ => None,
    }
}

fn number(value: &[u8]) -> Option<u32> {
    if value.is_empty() || !value.iter().all(u8::is_ascii_digit) {
        return None;
    }
    std::str::from_utf8(value).ok()?.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// pi 实际发的那一条。
    #[test]
    fn 解析_pi_的控制段() {
        let control = parse_control(b"a=T,f=100,q=2,C=1,c=40,r=6,i=3525535711,m=1").expect("合法");
        assert_eq!(control.action, Some(b'T'));
        assert_eq!(control.format, Some(100));
        assert_eq!(control.quiet, 2);
        assert!(control.keep_cursor);
        assert_eq!(control.columns, Some(40));
        assert_eq!(control.rows, Some(6));
        assert_eq!(control.image_id, Some(3525535711));
        assert!(control.more);
    }

    /// 全屏模式的重放:`a=p` 带裁切。
    #[test]
    fn 解析裁切() {
        let control = parse_control(b"a=p,q=2,i=9,c=40,r=3,y=100,h=200,C=1").expect("合法");
        assert_eq!(control.action, Some(b'p'));
        assert_eq!((control.src_y, control.src_h), (100, 200));
    }

    #[test]
    fn 后续分块只有_m() {
        let control = parse_control(b"m=0").expect("合法");
        assert_eq!(control, Control::default());
        assert!(parse_control(b"m=1").expect("合法").more);
    }

    #[test]
    fn 不认识的键跳过() {
        let control = parse_control(b"a=d,z=-1,X=3,Y=4,V=-2,foo=bar").expect("合法");
        assert_eq!(control.action, Some(b'd'));
    }

    #[test]
    fn 写坏了整条作废() {
        for raw in [
            &b"i=abc"[..],
            b"a=TT",
            b"c=-1",
            b"i=99999999999",
            b"a",
            b"f=",
        ] {
            assert_eq!(parse_control(raw), None, "{}", String::from_utf8_lossy(raw));
        }
        // 空段不算错:`a=t,,i=1` 与结尾的逗号都见过
        assert!(parse_control(b"a=t,,i=1,").is_some());
    }

    #[test]
    fn 虚拟与相对摆放都标成不支持() {
        assert!(parse_control(b"U=1").unwrap().unsupported_placement);
        assert!(parse_control(b"P=3,Q=4").unwrap().unsupported_placement);
        assert!(!parse_control(b"U=0").unwrap().unsupported_placement);
    }

    #[test]
    fn 切分正文() {
        assert_eq!(split_body(b"a=T;abc"), (&b"a=T"[..], &b"abc"[..]));
        assert_eq!(split_body(b"a=d,d=A"), (&b"a=d,d=A"[..], &b""[..]));
        // payload 里再有分号也不再切
        assert_eq!(split_body(b"m=1;a;b"), (&b"m=1"[..], &b"a;b"[..]));
    }
}
