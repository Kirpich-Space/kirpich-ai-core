# -*- coding: utf-8 -*-
"""Живая проверка ПОСЛЕ записи 0.2 в реестр.

Идёт через Scheduler (`/tasks` не используется — он запустил бы полный
Video-пайплайн с рендером), поэтому температуру подставляет сам реестр:
скрипт её не задаёт. Если бы значение не доехало, запрос ушёл бы с
умолчанием провайдера — но проверяется здесь не это, а что контур цел:
термин извлекается и Openverse отвечает.
"""
import json
import urllib.parse
import urllib.request

import measure  # промпт, санитайзер и сужение — те же, что в пайплайне

TEXT = "сделай видео про горное озеро"

raw, secs = measure.ask(f"Задача: {TEXT}\nКлючевые слова:", measure.SYSTEM, 0.2)
term = measure.sanitize(raw)
print(f"вход:  {TEXT}")
print(f"сырой ответ модели: {raw.strip()!r}  ({secs} с)")
print(f"термин после sanitize: {term!r}")

for variant in measure.narrowing_variants(term or TEXT):
    count, got = measure.openverse_count(variant)
    print(f"Openverse «{variant}»: result_count={count}, отдано={got}")
    if got:
        break

status = json.load(urllib.request.urlopen("http://127.0.0.1:4545/status", timeout=10))
print("backend:", {k: status[k] for k in ("used_model_memory_mb", "running_tasks")})
models = json.load(urllib.request.urlopen("http://127.0.0.1:4545/models", timeout=10))
for m in models:
    if m["model"] == "Fable9B":
        print("GET /models -> Fable9B.temperature =", m["temperature"])
