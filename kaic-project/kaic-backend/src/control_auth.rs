//! Аутентификация Control Center API.
//!
//! Зачем это существует. До этой правки Control Center слушал
//! `127.0.0.1:4545` без единой проверки и с `CorsLayer::permissive()`.
//! «Только localhost» не является защитой от браузера: любая страница,
//! открытая в браузере пользователя, живёт на его машине и обращается к
//! `127.0.0.1` как к своему соседу. Разрешающий CORS дополнительно снимал
//! запрет на чтение ответа, то есть страница могла не только поставить
//! задачу, но и прочитать очередь задач пользователя.
//!
//! Почему токен, а не «доверенный origin». Origin подделывается не браузером,
//! а любым не-браузерным клиентом: `curl -H 'Origin: …'` пройдёт любую
//! проверку origin. CORS ограничивает браузер и только браузер; он —
//! второй рубеж, а не первый. Первый рубеж — предъявленный секрет.
//!
//! Почему переменная окружения, а не поле в YAML. В этом проекте уже есть
//! ровно такая конвенция и она работает: Telegram Bridge читает
//! `TELOXIDE_TOKEN` из окружения, а `config/telegram.yaml` прямо пишет
//! «ТОКЕН ЗДЕСЬ НЕ ХРАНИТСЯ». Заводить второй способ хранения секрета —
//! значит завести второй способ его потерять. Переиспользуем конвенцию.
//!
//! Почему тип, а не строка. [`ControlToken`] — приватное поле без
//! публичного конструктора, а [`crate::control_center_api::build_router`]
//! принимает его обязательным аргументом. Собрать роутер без токена нельзя
//! не потому, что об этом помнят, а потому, что нечего подставить. Тот же
//! приём, что у `ApprovedToolCall` в гейте инструментов и у `AdmittedAsset`
//! в медиа-гейте.

use std::sync::Arc;

use axum::extract::{Request, State};
use axum::http::{header, StatusCode};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};

/// Имя переменной окружения, из которой читается секрет.
pub const ENV_VAR: &str = "KAIC_CONTROL_TOKEN";

/// Минимальная длина. Не «лучшая практика», а граница, ниже которой секрет
/// перебирается локально быстрее, чем человек заметит нагрузку: Control
/// Center отвечает на запрос за единицы миллисекунд и не имеет ни счётчика
/// попыток, ни задержки.
pub const MIN_LEN: usize = 16;

/// Секрет Control Center API.
///
/// Поле приватно, публичного конструктора нет. Единственные способы получить
/// значение — [`ControlToken::from_env`] (в бою) и [`ControlToken::for_test`]
/// (только под `cfg(test)`). Строку в роутер подставить нельзя: не тот тип.
#[derive(Clone)]
pub struct ControlToken(Arc<String>);

/// `Debug` пишется руками и намеренно НЕ печатает секрет.
///
/// Производный `Debug` вывел бы токен в первый же `tracing::debug!` или в
/// текст паники, а логи переживают процесс и попадают в отчёты. Тип, который
/// нельзя случайно распечатать, — та же гарантия уровня типа, что и всё
/// остальное здесь.
impl std::fmt::Debug for ControlToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ControlToken(<{} символов, скрыт>)", self.0.chars().count())
    }
}

impl ControlToken {
    /// Читает секрет из окружения.
    ///
    /// Отсутствие переменной — ошибка, а не «работать без пароля». Анонимного
    /// режима нет вовсе: его наличие означало бы, что дыру можно вернуть
    /// забывчивостью, а вся эта правка ровно про то, чтобы так было нельзя.
    pub fn from_env() -> anyhow::Result<Self> {
        let raw = std::env::var(ENV_VAR).map_err(|_| {
            anyhow::anyhow!(
                "переменная {ENV_VAR} не задана. Control Center API не поднимается \
                 без секрета: без него любая страница, открытая в браузере, ставит \
                 задачи от вашего имени. Задайте её так же, как TELOXIDE_TOKEN, \
                 например:\n    $env:{ENV_VAR} = \"<не менее {MIN_LEN} символов>\""
            )
        })?;
        Self::from_raw(raw)
    }

    fn from_raw(raw: String) -> anyhow::Result<Self> {
        let trimmed = raw.trim();
        if trimmed.chars().count() < MIN_LEN {
            anyhow::bail!(
                "{ENV_VAR} короче {MIN_LEN} символов ({} символов). \
                 Короткий секрет перебирается локально быстрее, чем это будет замечено",
                trimmed.chars().count()
            );
        }
        Ok(Self(Arc::new(trimmed.to_string())))
    }

    /// Только для тестов: они не должны трогать окружение процесса,
    /// потому что тесты идут параллельно в одном процессе, а `set_var`
    /// в этом случае — гонка, а не подготовка.
    #[cfg(test)]
    pub fn for_test(value: &str) -> Self {
        Self(Arc::new(value.to_string()))
    }

    /// Сравнение за постоянное время.
    ///
    /// Обычное `==` на строках выходит на первом несовпавшем байте, и
    /// разница во времени ответа выдаёт префикс. Локальность здесь не
    /// оправдание: страница в браузере измеряет тайминг локального запроса
    /// точнее, чем удалённого.
    fn matches(&self, presented: &str) -> bool {
        let expected = self.0.as_bytes();
        let got = presented.as_bytes();
        // Длину скрывать не пытаемся — она и так видна по трафику; скрываем
        // позицию первого несовпадения, а это и есть то, что даёт перебор.
        let mut diff: u8 = (expected.len() != got.len()) as u8;
        let n = expected.len().max(got.len());
        for i in 0..n {
            let a = expected.get(i).copied().unwrap_or(0);
            let b = got.get(i).copied().unwrap_or(0);
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Проверяет заголовок `Authorization: Bearer <token>`.
    ///
    /// Схема сравнивается регистронезависимо (RFC 7235 требует именно так),
    /// сам секрет — побайтово.
    pub fn accepts(&self, header_value: Option<&str>) -> bool {
        let Some(value) = header_value else {
            return false;
        };
        let Some((scheme, rest)) = value.split_once(' ') else {
            return false;
        };
        if !scheme.eq_ignore_ascii_case("bearer") {
            return false;
        }
        self.matches(rest.trim())
    }
}

/// Middleware: без верного `Authorization` дальше маршрута не пускает.
///
/// Ответ — 401 с `WWW-Authenticate`, а не 403: клиент не «не имеет права»,
/// он «не представился». Разница видна инструментам и людям.
pub async fn require_token(
    State(token): State<ControlToken>,
    request: Request,
    next: Next,
) -> Response {
    let presented = request
        .headers()
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());

    if !token.accepts(presented.as_deref()) {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::WWW_AUTHENTICATE, "Bearer realm=\"kaic-control-center\"")],
            "Control Center API требует Authorization: Bearer <KAIC_CONTROL_TOKEN>",
        )
            .into_response();
    }

    next.run(request).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debug_does_not_print_the_secret() {
        // Иначе токен уедет в лог первым же `tracing::debug!` или в текст паники.
        let token = ControlToken::for_test("supersecretvalue0123");
        let shown = format!("{token:?}");
        assert!(
            !shown.contains("supersecretvalue0123"),
            "Debug напечатал секрет: {shown}"
        );
        assert!(shown.contains("скрыт"), "Debug должен говорить, что скрыл: {shown}");
    }

    #[test]
    fn a_missing_header_is_refused() {
        let token = ControlToken::for_test("0123456789abcdef0123");
        assert!(!token.accepts(None));
    }

    #[test]
    fn a_wrong_token_is_refused() {
        let token = ControlToken::for_test("0123456789abcdef0123");
        assert!(!token.accepts(Some("Bearer 0123456789abcdef0124")));
    }

    #[test]
    fn a_correct_token_is_accepted() {
        let token = ControlToken::for_test("0123456789abcdef0123");
        assert!(token.accepts(Some("Bearer 0123456789abcdef0123")));
    }

    #[test]
    fn the_scheme_is_case_insensitive_but_the_secret_is_not() {
        let token = ControlToken::for_test("0123456789abcdefABCD");
        assert!(token.accepts(Some("bearer 0123456789abcdefABCD")));
        assert!(token.accepts(Some("BEARER 0123456789abcdefABCD")));
        // Секрет — побайтово: смена регистра это другой секрет.
        assert!(!token.accepts(Some("Bearer 0123456789abcdefabcd")));
    }

    #[test]
    fn a_bare_token_without_the_scheme_is_refused() {
        // Иначе `Authorization: <token>` работал бы «по доброте», и на него
        // начали бы полагаться — а половина клиентов шлёт схему.
        let token = ControlToken::for_test("0123456789abcdef0123");
        assert!(!token.accepts(Some("0123456789abcdef0123")));
    }

    #[test]
    fn a_prefix_of_the_secret_is_refused() {
        // Проверка длины внутри сравнения за постоянное время должна ловить
        // и укороченный, и удлинённый вариант.
        let token = ControlToken::for_test("0123456789abcdef0123");
        assert!(!token.accepts(Some("Bearer 0123456789abcdef012")));
        assert!(!token.accepts(Some("Bearer 0123456789abcdef01234")));
    }

    #[test]
    fn a_short_secret_is_refused_at_construction() {
        let err = ControlToken::from_raw("короткий".to_string()).unwrap_err();
        assert!(
            err.to_string().contains("короче"),
            "ошибка должна называть причину, а не быть безымянной: {err}"
        );
    }

    #[test]
    fn a_secret_of_exactly_the_minimum_length_is_accepted() {
        // Граница включительна — иначе документированная минимальная длина
        // не совпадала бы с проверяемой.
        let value = "a".repeat(MIN_LEN);
        assert!(ControlToken::from_raw(value).is_ok());
    }

    #[test]
    fn surrounding_whitespace_does_not_become_part_of_the_secret() {
        // `$env:KAIC_CONTROL_TOKEN = "…"` из PowerShell легко приносит \r.
        let token = ControlToken::from_raw(format!("  {}  \r\n", "a".repeat(MIN_LEN))).unwrap();
        assert!(token.accepts(Some(&format!("Bearer {}", "a".repeat(MIN_LEN)))));
    }
}
