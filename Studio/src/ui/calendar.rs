//! Календарные границы местной даты из системной date, без подмены UTC.
use crate::storage::Chat;
use std::process::Command;

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Group {
    Today,
    Week,
    Older,
    Undated,
}
impl Group {
    pub fn label_for(self, lang: super::i18n::Lang) -> &'static str {
        match self {
            Self::Today => lang.t("Today", "Сегодня"),
            Self::Week => lang.t("This week", "На этой неделе"),
            Self::Older => lang.t("Older", "Раньше"),
            Self::Undated => lang.t("Undated", "Без даты"),
        }
    }
}
#[derive(Clone, Copy)]
pub(super) struct Cutoffs {
    pub today: u64,
    pub week: u64,
}
impl Cutoffs {
    pub fn local() -> Option<Self> {
        let output = Command::new("date").arg("+%Y-%m-%d %u").output().ok()?;
        if !output.status.success() {
            return None;
        }
        let text = String::from_utf8(output.stdout).ok()?;
        let (day, weekday) = text.trim().split_once(' ')?;
        let offset = weekday.parse::<u8>().ok()?.checked_sub(1)?;
        let epoch = |value: String| -> Option<u64> {
            let result = Command::new("date")
                .args(["-d", &value, "+%s"])
                .output()
                .ok()?;
            if !result.status.success() {
                return None;
            }
            String::from_utf8(result.stdout).ok()?.trim().parse().ok()
        };
        Some(Self {
            today: epoch(format!("{day} 00:00:00"))?,
            week: epoch(format!("{day} -{offset} days 00:00:00"))?,
        })
    }
    pub fn group(self, timestamp: Option<u64>) -> Group {
        match timestamp {
            Some(t) if t >= self.today => Group::Today,
            Some(t) if t >= self.week => Group::Week,
            Some(_) => Group::Older,
            None => Group::Undated,
        }
    }
}
pub(super) fn rows(chats: &[Chat], cutoffs: Option<Cutoffs>) -> Vec<(usize, Group)> {
    let mut rows: Vec<_> = chats
        .iter()
        .enumerate()
        .map(|(i, c)| {
            (
                i,
                cutoffs
                    .map(|v| v.group(c.updated_at.or(c.created_at)))
                    .unwrap_or(Group::Undated),
            )
        })
        .collect();
    rows.sort_by_key(|&(i, g)| {
        (
            g,
            std::cmp::Reverse(chats[i].updated_at.or(chats[i].created_at)),
        )
    });
    rows
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn finish_calendar_boundaries_and_original_indices() {
        let c = Cutoffs {
            today: 100,
            week: 50,
        };
        assert_eq!(c.group(Some(100)), Group::Today);
        assert_eq!(c.group(Some(99)), Group::Week);
        assert_eq!(c.group(Some(50)), Group::Week);
        assert_eq!(c.group(Some(49)), Group::Older);
        assert_eq!(c.group(None), Group::Undated);
        let mut chats = vec![Chat::new(), Chat::new(), Chat::new()];
        chats[0].updated_at = Some(60);
        chats[1].updated_at = Some(120);
        chats[2].updated_at = Some(80);
        assert_eq!(
            rows(&chats, Some(c))
                .iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            vec![1, 2, 0]
        );
    }
    #[test]
    fn finish_old_history_keeps_unknown_dates() {
        let c: Chat = serde_json::from_value(
            serde_json::json!({"id":uuid::Uuid::nil(),"title":"old","messages":[]}),
        )
        .unwrap();
        assert_eq!(c.created_at, None);
        assert_eq!(c.updated_at, None);
        assert_eq!(
            rows(
                &[c],
                Some(Cutoffs {
                    today: 100,
                    week: 50
                })
            )[0]
            .1,
            Group::Undated
        );
    }
}
