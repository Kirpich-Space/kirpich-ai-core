# -*- coding: utf-8 -*-
"""Свод трёх замеров по критериям, объявленным ДО замера."""
import io
import json
import statistics

BASE = [  # замер 2026-08-09 при temperature 0.2, из RESULTS предыдущей задачи
    ("mountain lake", ["горное озеро", "mountain lake", "mountain lake"], [14, 240, 240]),
    ("сделай видео про горное озеро", ["горное озеро"] * 3, [14, 14, 14]),
    ("зимний лес", ["зимний лес"] * 3, [58, 58, 58]),
    ("make a short video about a snowy forest at dawn",
     ["снежный лес на", "snowy forest at", "snowy forest at"], [1, 240, 240]),
    ("хочу ролик секунд на двадцать, где показан старый маяк на скалистом берегу",
     ["Old lighthouse on", "старый маяк на", "старый маяк на"], [240, 4, 4]),
    ("please put together a clip showing a busy city street in the rain, nothing fancy",
     ["rainy city street", "busy city street", "улица под дождем"], [240, 240, 61]),
]

STOPWORDS = {
    "a", "an", "the", "of", "on", "in", "at", "to", "for", "with", "by", "from",
    "over", "under", "into", "near", "and", "or",
    "в", "во", "на", "над", "под", "у", "к", "ко", "с", "со", "из", "от", "до",
    "по", "за", "при", "про", "для", "и", "или", "около", "возле",
}


def is_english(term):
    return not any("а" <= c.lower() <= "я" or c.lower() == "ё" for c in term)


def load(path):
    rows = json.load(io.open(path, encoding="utf-8"))
    by_input = {}
    for r in rows:
        by_input.setdefault(r["input"], []).append(r)
    return by_input


def summarise(name, terms, counts):
    tails = [t for t in terms if t and t.split() and t.split()[-1].lower() in STOPWORDS
             and len(t.split()) > 1]
    non_english = [t for t in terms if t and not is_english(t)]
    zeros = [c for c in counts if c == 0]
    print(f"  {name}: медиана={statistics.median(counts):.0f}  "
          f"нулевых={len(zeros)}  хвостов={len(tails)}  нерусских_нет={not non_english}")
    return tails, non_english, zeros, counts


print("=== ПО ВХОДАМ ===")
step1, step2 = load("extract_step1.json"), load("extract_step2.json")
all_base, all_s1, all_s2 = [], [], []
b_tails = b_ne = s1_tails = s1_ne = s2_tails = s2_ne = 0
for text, bterms, bcounts in BASE:
    r1 = step1[text]
    r2 = step2[text]
    c1 = [r["result_count"] for r in r1]
    c2 = [r["result_count"] for r in r2]
    t1 = [r["term"] for r in r1]
    t2 = [r["term"] for r in r2]
    all_base += bcounts
    all_s1 += c1
    all_s2 += c2
    for terms, cnt in ((bterms, "b"), (t1, "1"), (t2, "2")):
        tails = sum(1 for t in terms if t and len(t.split()) > 1
                    and t.split()[-1].lower() in STOPWORDS)
        ne = sum(1 for t in terms if t and not is_english(t))
        if cnt == "b":
            b_tails += tails; b_ne += ne
        elif cnt == "1":
            s1_tails += tails; s1_ne += ne
        else:
            s2_tails += tails; s2_ne += ne
    print(f"\n[{text[:48]}]")
    print(f"  база   : {bcounts}  {bterms}")
    print(f"  шаг 1  : {c1}  {t1}")
    print(f"  шаг 2  : {c2}  {t2}")

print("\n=== СВОД ===")
for label, vals, tails, ne in (("база", all_base, b_tails, b_ne),
                               ("шаг 1", all_s1, s1_tails, s1_ne),
                               ("шаг 2", all_s2, s2_tails, s2_ne)):
    print(f"{label:6}: медиана={statistics.median(vals):>6.0f}  "
          f"среднее={statistics.mean(vals):>7.1f}  нулевых={sum(1 for v in vals if v == 0)}  "
          f"хвостов={tails}/18  неанглийских={ne}/18")
