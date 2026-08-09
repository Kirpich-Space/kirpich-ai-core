# -*- coding: utf-8 -*-
"""Замер temperature на живых контурах Fable9B: извлечение поискового
термина и категория Simple.

Промпт, потолки длины, санитайзер и порядок сужения — не сочинены, а
перенесены один в один из кода пайплайна:
  src/media/query.rs      build_messages / sanitize / narrowing_variants
  src/control_center_api.rs  REQUESTED_LICENSES / SEARCH_PAGE_SIZE
  src/media/openverse.rs     API_IMAGES / USER_AGENT
Иначе измерялся бы не тот контур.

Обращение к LM Studio прямое: пайплайн KAIC не используется, потому что
температуру надо задавать явно, а не через реестр.
"""
import json
import sys
import time
import urllib.parse
import urllib.request

LM_STUDIO = "http://127.0.0.1:1234/v1/chat/completions"
FABLE9B = "qwable-9b-claude-fable-5-obliterated-i1"
API_IMAGES = "https://api.openverse.org/v1/images/"
USER_AGENT = "KAIC-media-pipeline/0.1"
REQUESTED_LICENSES = "cc0,pdm,by"
SEARCH_PAGE_SIZE = 5

# --- query.rs: build_messages ------------------------------------------------
SYSTEM = ("Ты выделяешь поисковый запрос для банка фотографий. "
          "Ответь ТОЛЬКО названием предмета съёмки: одно-два слова, связная фраза "
          "(например «горное озеро», «зимний лес»). "
          "НЕ перечисляй синонимы через пробел. "
          "Без глаголов-команд, без слов «видео», «фото», «сделай». "
          "Без кавычек, без пояснений, без точки в конце.")

MAX_TERM_CHARS = 60   # query.rs
MAX_TERM_WORDS = 3    # query.rs


TRAILING_STOPWORDS = {
    "a","an","the","of","on","in","at","to","for","with","by","from","over",
    "under","into","near","and","or",
    "в","во","на","над","под","у","к","ко","с","со","из","от","до","по","за",
    "при","про","для","и","или","около","возле",
}


def strip_trailing_stopwords(term):
    words = term.split()
    while len(words) > 1 and words[-1].lower() in TRAILING_STOPWORDS:
        words.pop()
    return " ".join(words) if words else term

TRIM_CHARS = set(' \t\n\r"\'«»-*.:•')


def sanitize(raw):
    """Порт query.rs::sanitize. None — извлечение не удалось."""
    lines = [l.strip() for l in raw.splitlines()]
    lines = [l for l in lines if l]
    if not lines:
        return None
    line = lines[-1]                      # next_back(): последняя непустая

    cleaned = line.strip("".join(TRIM_CHARS))
    cleaned = "".join(c for c in cleaned
                      if c.isalnum() or c.isspace() or c == "-")

    words = cleaned.split()
    if not words:
        return None
    if len(cleaned) > MAX_TERM_CHARS:     # модель пересказала задачу
        return None
    term = " ".join(words[:MAX_TERM_WORDS])
    term = strip_trailing_stopwords(term)
    return term if term.strip() else None


def narrowing_variants(term):
    """Порт query.rs::narrowing_variants — от узкого к широкому."""
    words = term.split()
    out = []
    for take in range(len(words), 0, -1):
        v = " ".join(words[:take])
        if v not in out:
            out.append(v)
    return out


def ask(prompt_user, system, temperature, timeout=180):
    # system=None — сообщений роли system нет вовсе. Именно так пайплайн
    # обслуживает категорию Simple: context_to_messages отдаёт только то,
    # что накопилось в задаче, и системного промпта там не появляется
    # (control_center_api.rs::context_to_messages). Подставить свой значило
    # бы измерить не тот контур.
    msgs = []
    if system is not None:
        msgs.append({"role": "system", "content": system})
    msgs.append({"role": "user", "content": prompt_user})
    body = {"model": FABLE9B, "messages": msgs}
    if temperature is not None:           # null = поле не отправляется вовсе
        body["temperature"] = temperature
    req = urllib.request.Request(LM_STUDIO, data=json.dumps(body).encode("utf-8"),
                                 headers={"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=timeout) as r:
        data = json.load(r)
    return data["choices"][0]["message"]["content"], round(time.time() - t0, 1)


def openverse_count(query):
    url = API_IMAGES + "?" + urllib.parse.urlencode({
        "q": query, "license": REQUESTED_LICENSES, "page_size": SEARCH_PAGE_SIZE})
    req = urllib.request.Request(url, headers={"User-Agent": USER_AGENT,
                                               "Accept": "application/json"})
    with urllib.request.urlopen(req, timeout=60) as r:
        d = json.load(r)
    return d.get("result_count", 0), len(d.get("results", []) or [])


INPUTS = [
    "mountain lake",                                  # подтверждён живьём
    "сделай видео про горное озеро",                  # подтверждён живьём
    "зимний лес",                                     # короткая русская
    "make a short video about a snowy forest at dawn",  # развёрнутая англ.
    "хочу ролик секунд на двадцать, где показан старый маяк на скалистом берегу",  # с деталями
    "please put together a clip showing a busy city street in the rain, nothing fancy",  # с деталями
]


def run_extraction(temperature, repeats=3):
    rows = []
    for text in INPUTS:
        for rep in range(1, repeats + 1):
            user = f"Задача: {text}\nКлючевые слова:"
            try:
                raw, secs = ask(user, SYSTEM, temperature)
            except Exception as e:
                rows.append({"input": text, "rep": rep, "error": str(e)[:80]})
                continue
            term = sanitize(raw)
            fallback = None
            if term is None:
                term_used = text                       # уровень 2: дословный текст
                fallback = "неправдоподобный ответ -> дословный текст"
            else:
                term_used = term
            attempts = narrowing_variants(term_used)
            if text not in attempts:
                attempts.append(text)                  # уровень 3, как в пайплайне
            count, got = 0, 0
            used = None
            for i, a in enumerate(attempts):
                try:
                    count, got = openverse_count(a)
                except Exception as e:
                    rows.append({"input": text, "rep": rep, "error": "openverse: " + str(e)[:60]})
                    break
                if got > 0:
                    used = a
                    if i > 0 and fallback is None:
                        fallback = f"сужение до «{a}»"
                    break
                time.sleep(1.0)
            rows.append({
                "input": text, "rep": rep, "temp": temperature,
                "raw": raw.strip()[:120], "term": term, "used_query": used,
                "result_count": count, "returned": got,
                "fallback": fallback, "gen_s": secs,
            })
            time.sleep(1.0)                            # к Openverse экономно
    return rows


SIMPLE_QUESTIONS = [
    "Привет! Как дела?",
    "Что такое оперативная память, коротко?",
    "Посоветуй, чем занять вечер дома.",
    "Сколько будет 17 умножить на 4?",
]


def run_simple(temperature, repeats=3):
    rows = []
    for q in SIMPLE_QUESTIONS:
        for rep in range(1, repeats + 1):
            try:
                raw, secs = ask(q, None, temperature)
            except Exception as e:
                rows.append({"q": q, "rep": rep, "error": str(e)[:80]})
                continue
            rows.append({"q": q, "rep": rep, "temp": temperature,
                         "answer": raw.strip(), "gen_s": secs})
    return rows


if __name__ == "__main__":
    what = sys.argv[1]
    temp = None if sys.argv[2] == "null" else float(sys.argv[2])
    rows = run_extraction(temp) if what == "extract" else run_simple(temp)
    name = f"{what}_{sys.argv[2]}.json"
    open(name, "w", encoding="utf-8").write(json.dumps(rows, ensure_ascii=False, indent=2))
    print(f"записано: {name}, строк {len(rows)}")
