# -*- coding: utf-8 -*-
"""Машинные признаки вырождения ответов Simple. Оценки по шкале здесь нет
намеренно: приводятся только факты, решение о стиле — за архитектором."""
import io
import json

for name in ("simple_null.json", "simple_0.2.json"):
    rows = json.load(io.open(name, encoding="utf-8"))
    print("===", name, "===")
    empty = short = loops = unfinished = 0
    for r in rows:
        a = r.get("answer", "").strip()
        if not a:
            empty += 1
        if 0 < len(a) < 15:
            short += 1
        w = a.split()
        if any(w[i:i + 4] == w[i + 4:i + 8] == w[i + 8:i + 12]
               for i in range(max(0, len(w) - 12))):
            loops += 1
        ends_ok = bool(a) and (a[-1] in ".!?)»:0123456789*" or a.endswith("```"))
        if not ends_ok:
            unfinished += 1
            print(f"   НЕЗАВЕРШЁН: {r['q'][:24]} r{r['rep']} хвост={a[-40:]!r}")
    print(f"   пустых={empty}, короче 15 симв={short}, зацикленных={loops}, "
          f"без завершающего знака={unfinished}, всего={len(rows)}")
