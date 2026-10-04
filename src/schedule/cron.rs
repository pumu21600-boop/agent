//! P22-B 定时任务：5 段 cron 解析与下次触发计算（本地时区）
//!
//! 支持 `*` / 数字 / `a-b` 范围 / `*/n` 与 `a-b/n` 步进 / 逗号列表，字段区间
//! 分 0-59、时 0-23、日 1-31、月 1-12、周 0-7（0 和 7 都算周日）。
//! 日与周同时受限时按 Vixie cron 语义取**或**（任一生效即触发）。
//! 不引 cron crate：解析 + next_after 是纯函数，自己写可单测、零依赖
//! （本机到 crates.io 的出口也不稳定，能不自依赖就不自依赖）。

use chrono::{DateTime, Datelike, Local, NaiveDate};

/// 一段 cron 字段的取值集合。
#[derive(Debug, Clone, PartialEq)]
pub struct CronSpec {
    minutes: Vec<u32>,
    hours: Vec<u32>,
    /// 每月的第几天（1-31）
    dom: Vec<u32>,
    months: Vec<u32>,
    /// 周几（0=周日 … 6=周六；7 已归一为 0）
    dow: Vec<u32>,
}

/// 解析 5 段 cron 表达式。错误信息人话（设置面板直接展示）。
pub fn parse(expr: &str) -> Result<CronSpec, String> {
    let parts: Vec<&str> = expr.split_whitespace().collect();
    if parts.len() != 5 {
        return Err(format!("cron 需要 5 段（分 时 日 月 周），收到 {} 段", parts.len()));
    }
    let minutes = field(parts[0], 0, 59, "分")?;
    let hours = field(parts[1], 0, 23, "时")?;
    let dom = field(parts[2], 1, 31, "日")?;
    let months = field(parts[3], 1, 12, "月")?;
    // 周：0 和 7 都算周日——显式写了 7 才归一成 0（不能反过来给不含 0 的规格补周日）
    let dow_raw = field(parts[4], 0, 7, "周")?;
    let mut dow: Vec<u32> = dow_raw.iter().map(|d| if *d == 7 { 0 } else { *d }).collect();
    dow.sort_unstable();
    dow.dedup();
    Ok(CronSpec { minutes, hours, dom, months, dow })
}

/// 解析一段字段：`*` / `*/n` / `a` / `a-b` / `a-b/n` / 逗号组合。
fn field(spec: &str, min: u32, max: u32, label: &str) -> Result<Vec<u32>, String> {
    let mut out: Vec<u32> = Vec::new();
    for part in spec.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("cron {label}段有空项"));
        }
        let (range_part, step) = match part.split_once('/') {
            Some((r, s)) => {
                let step: u32 = s
                    .parse()
                    .map_err(|_| format!("cron {label}段步进不是数字: {s}"))?;
                if step == 0 {
                    return Err(format!("cron {label}段步进不能为 0"));
                }
                (r, step)
            }
            None => (part, 1),
        };
        let (lo, hi) = if range_part == "*" {
            (min, max)
        } else if let Some((a, b)) = range_part.split_once('-') {
            let a: u32 = a
                .trim()
                .parse()
                .map_err(|_| format!("cron {label}段不是数字: {a}"))?;
            let b: u32 = b
                .trim()
                .parse()
                .map_err(|_| format!("cron {label}段不是数字: {b}"))?;
            (a, b)
        } else {
            let v: u32 = range_part
                .parse()
                .map_err(|_| format!("cron {label}段不是数字: {range_part}"))?;
            // 单值 + 步进（如 5/10）= 从该值起到段尾（Vixie 语义）
            if part.contains('/') { (v, max) } else { (v, v) }
        };
        if lo < min || hi > max || lo > hi {
            return Err(format!("cron {label}段越界: {part}（允许 {min}-{max}）"));
        }
        let mut v = lo;
        while v <= hi {
            out.push(v);
            v += step;
        }
    }
    if out.is_empty() {
        return Err(format!("cron {label}段没有取值"));
    }
    out.sort_unstable();
    out.dedup();
    Ok(out)
}

impl CronSpec {
    /// `from` 之后（严格大于）的下一个触发时刻（本地时区，分钟精度）。
    /// 4 年零 1 天内找不到（如 2 月 29 日且错过闰年）返回 None。
    pub fn next_after(&self, from: DateTime<Local>) -> Option<DateTime<Local>> {
        // 从下一分钟开始找（同分钟不重复触发）
        let start = from + chrono::Duration::minutes(1);
        let mut day = start.date_naive();
        for _ in 0..=1461 {
            if self.months.contains(&day.month()) && self.day_matches(day) {
                for h in &self.hours {
                    for m in &self.minutes {
                        let Some(naive) = day.and_hms_opt(*h, *m, 0) else { continue };
                        let cand = match naive.and_local_timezone(Local) {
                            chrono::LocalResult::Single(t) => t,
                            chrono::LocalResult::Ambiguous(t, _) => t, //  ambiguous 取早的那个
                            chrono::LocalResult::None => continue,    //  DST 跳过的钟点不存在
                        };
                        if cand > from {
                            return Some(cand);
                        }
                    }
                }
            }
            day = day.succ_opt()?;
        }
        None
    }

    /// 日/周匹配（两者都受限时取或，与 Vixie cron 一致）。
    fn day_matches(&self, day: NaiveDate) -> bool {
        let dom_hit = self.dom.contains(&day.day());
        let dow_hit = self.dow.contains(&day.weekday().num_days_from_sunday());
        let dom_any = self.dom.len() >= 31;
        let dow_any = self.dow.len() >= 7;
        match (dom_any, dow_any) {
            (false, false) => dom_hit || dow_hit,
            _ => dom_hit && dow_hit,
        }
    }

    /// 人话描述（设置面板展示用，一句）：常见形态紧凑（每天 09:00 / 工作日 09:00 /
    /// 每 15 分钟 / 每月 1 日 09:00），非常见形态退化为字段枚举（全字段用「每」省过）。
    pub fn describe(&self) -> String {
        let full_min = is_full(&self.minutes, 0, 59);
        let full_hour = is_full(&self.hours, 0, 23);
        let time = if full_min && full_hour {
            "每分钟".to_string()
        } else if full_min {
            // 等步进（*/n）→ 每 n 分钟；否则枚举分钟
            match even_step(&self.minutes) {
                Some(step) if self.minutes[0] == 0 => format!("每 {step} 分钟"),
                _ => format!("每小时 {} 分", list(&self.minutes)),
            }
        } else if full_hour {
            // 等步进分钟（0,30 → 每 30 分钟；0,15,30,45 → 每 15 分钟）
            match even_step(&self.minutes) {
                Some(step) if self.minutes[0] == 0 => format!("每 {step} 分钟"),
                _ => format!("每小时 {} 分", list(&self.minutes)),
            }
        } else if self.hours.len() == 1 && self.minutes.len() == 1 {
            format!("{:02}:{:02}", self.hours[0], self.minutes[0])
        } else {
            format!("{} 时 {} 分", list(&self.hours), list(&self.minutes))
        };
        let mut parts: Vec<String> = Vec::new();
        if self.dow == [1, 2, 3, 4, 5] {
            parts.push("工作日".into());
        } else if !is_full(&self.dow, 0, 6) {
            parts.push(format!(
                "每周{}",
                self.dow.iter().map(|d| WEEK_CN[*d as usize]).collect::<Vec<_>>().join("")
            ));
        }
        if !is_full(&self.months, 1, 12) {
            // 月份受限 = 一年只在那几个月跑，说「每月」是错的（如 `25 20 4 10 *`
            // 实际每年 10 月 4 日一次），日也受限时并进去说「每年 X 月 Y 日」
            if !is_full(&self.dom, 1, 31) {
                parts.push(format!("每年 {} 月 {} 日", list(&self.months), list(&self.dom)));
            } else {
                parts.push(format!("每年 {} 月", list(&self.months)));
            }
        } else if !is_full(&self.dom, 1, 31) {
            parts.push(format!("每月 {} 日", list(&self.dom)));
        }
        if parts.is_empty() {
            // 无日期限制：「每 N 分钟」这类间隔说法自带每天义，只有具体钟点要补「每天」
            if time.starts_with('每') {
                time
            } else {
                format!("每天 {time}")
            }
        } else {
            format!("{} {}", parts.join(" "), time)
        }
    }
}

/// 字段是否覆盖整个区间（排序去重后首尾即判）。
fn is_full(v: &[u32], min: u32, max: u32) -> bool {
    v.len() as u32 == max - min + 1 && v.first() == Some(&min)
}

/// 等步进集合的步长（[0,15,30,45] → 15）；不等步返回 None。
fn even_step(v: &[u32]) -> Option<u32> {
    if v.len() < 2 {
        return None;
    }
    let step = v[1] - v[0];
    v.windows(2).all(|w| w[1] - w[0] == step).then_some(step)
}

fn list(v: &[u32]) -> String {
    v.iter().map(|x| x.to_string()).collect::<Vec<_>>().join(",")
}

const WEEK_CN: [&str; 7] = ["日", "一", "二", "三", "四", "五", "六"];

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    fn lt(y: i32, m: u32, d: u32, h: u32, mi: u32) -> DateTime<Local> {
        Local.with_ymd_and_hms(y, m, d, h, mi, 0).unwrap()
    }

    #[test]
    fn parse_accepts_common_shapes() {
        assert!(parse("0 9 * * *").is_ok());
        assert!(parse("*/15 * * * *").is_ok());
        assert!(parse("0 9-18 * * 1-5").is_ok());
        assert!(parse("30 8,12,18 * * *").is_ok());
        assert!(parse("0 0 1 */3 *").is_ok());
        assert!(parse("0 0 * * 7").is_ok(), "7 = 周日应接受");
        // 错误形态
        assert!(parse("0 9 * *").is_err(), "少一段");
        assert!(parse("60 9 * * *").is_err(), "分越界");
        assert!(parse("0 25 * * *").is_err(), "时越界");
        assert!(parse("0 9 * * 8").is_err(), "周越界");
        assert!(parse("*/0 9 * * *").is_err(), "步进 0");
        assert!(parse("a 9 * * *").is_err(), "非数字");
        assert!(parse("5-2 9 * * *").is_err(), "范围反了");
    }

    #[test]
    fn next_after_daily_and_interval() {
        let spec = parse("0 9 * * *").unwrap();
        // 当天 9 点前 → 当天 9 点；9 点整 → 次日 9 点（严格大于）
        assert_eq!(spec.next_after(lt(2026, 9, 24, 7, 30)), Some(lt(2026, 9, 24, 9, 0)));
        assert_eq!(spec.next_after(lt(2026, 9, 24, 9, 0)), Some(lt(2026, 9, 25, 9, 0)));
        // 错过 9 点 → 次日
        assert_eq!(spec.next_after(lt(2026, 9, 24, 18, 0)), Some(lt(2026, 9, 25, 9, 0)));
        let quarter = parse("*/15 * * * *").unwrap();
        assert_eq!(quarter.next_after(lt(2026, 9, 24, 9, 1)), Some(lt(2026, 9, 24, 9, 15)));
        assert_eq!(quarter.next_after(lt(2026, 9, 24, 9, 46)), Some(lt(2026, 9, 24, 10, 0)));
    }

    #[test]
    fn next_after_weekday_and_dow_or_dom() {
        // 2026-09-24 是周四；工作日上午 10 点
        let spec = parse("0 10 * * 1-5").unwrap();
        assert_eq!(spec.next_after(lt(2026, 9, 24, 8, 0)), Some(lt(2026, 9, 24, 10, 0)));
        // 周四 11 点 → 周五 10 点
        assert_eq!(spec.next_after(lt(2026, 9, 24, 11, 0)), Some(lt(2026, 9, 25, 10, 0)));
        // 周日都跳过：周六(9/26)晚 → 下周一(9/28)
        assert_eq!(spec.next_after(lt(2026, 9, 26, 20, 0)), Some(lt(2026, 9, 28, 10, 0)));
        // 日与周都受限 = 或：每月 1 号 或 每周一
        let or_spec = parse("0 10 1 * 1").unwrap();
        // 9/24 周四 11 点 → 下一个周一 9/28（或语义：不等 10/1）
        assert_eq!(or_spec.next_after(lt(2026, 9, 24, 11, 0)), Some(lt(2026, 9, 28, 10, 0)));
    }

    #[test]
    fn next_after_month_and_list() {
        // 每季度首日零点
        let spec = parse("0 0 1 1,4,7,10 *").unwrap();
        assert_eq!(spec.next_after(lt(2026, 9, 24, 12, 0)), Some(lt(2026, 10, 1, 0, 0)));
        // 列表小时
        let spec = parse("0 8,20 * * *").unwrap();
        assert_eq!(spec.next_after(lt(2026, 9, 24, 9, 0)), Some(lt(2026, 9, 24, 20, 0)));
        assert_eq!(spec.next_after(lt(2026, 9, 24, 21, 0)), Some(lt(2026, 9, 25, 8, 0)));
    }

    #[test]
    fn impossible_spec_returns_none() {
        // 2 月 30 日不存在 → None（不 panic、不死循环）
        let spec = parse("0 0 30 2 *").unwrap();
        assert!(spec.next_after(lt(2026, 9, 24, 0, 0)).is_none());
    }

    #[test]
    fn describe_is_compact_for_common_shapes() {
        assert_eq!(parse("* * * * *").unwrap().describe(), "每分钟");
        assert_eq!(parse("*/15 * * * *").unwrap().describe(), "每 15 分钟");
        assert_eq!(parse("0 9 * * *").unwrap().describe(), "每天 09:00");
        assert_eq!(parse("0 9 * * 1-5").unwrap().describe(), "工作日 09:00");
        assert_eq!(parse("30 8,20 * * *").unwrap().describe(), "每天 8,20 时 30 分");
        assert_eq!(parse("0 9 1 * *").unwrap().describe(), "每月 1 日 09:00");
        assert_eq!(parse("0 0 1 1,7 *").unwrap().describe(), "每年 1,7 月 1 日 00:00");
        // 月份受限+日不限：纯按月份（用户实锤 `25 20 4 10 *` 曾被说成「每月」，实际每年一次）
        assert_eq!(parse("0 9 * 3 *").unwrap().describe(), "每年 3 月 09:00");
        assert_eq!(parse("0 9 * * 1,3").unwrap().describe(), "每周一三 09:00");
        // 每小时整点（分钟只有 0）
        assert_eq!(parse("0 * * * *").unwrap().describe(), "每小时 0 分");
    }
}
