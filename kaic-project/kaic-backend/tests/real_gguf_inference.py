#!/usr/bin/env python3
"""Explicit integration test against an already running single-model llama-server.

Uses real CLI requests, Router, Scheduler and SQLite; no synthetic provider is started.
Model process lifecycle stays with its operator. Runtime artifacts stay outside Git.
"""

import argparse
import json
import socket
import sqlite3
from pathlib import Path
import urllib.error
import urllib.request

from cli_inference import BASE, HTTP, Kaic, check, wait_for


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("backend", type=Path)
    parser.add_argument("cli", type=Path)
    parser.add_argument("--models-dir", required=True, type=Path)
    parser.add_argument("--model-file", required=True, type=Path)
    parser.add_argument("--model-id", required=True)
    parser.add_argument("--footprint-mb", required=True, type=int)
    parser.add_argument("--budget-mb", required=True, type=int)
    parser.add_argument("--endpoint", default="http://127.0.0.1:18080")
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument("--timeout", type=float, default=180)
    args = parser.parse_args()
    check(0 < args.footprint_mb <= args.budget_mb <= 2**32 - 1,
          "нужны положительные footprint и budget, footprint <= budget")
    check(args.timeout > 0, "timeout должен быть положительным")
    with socket.socket() as probe:
        check(probe.connect_ex(("127.0.0.1", 4545)) != 0, "порт KAIC уже занят")
    model = args.model_file.resolve(strict=True)
    root = args.models_dir.resolve(strict=True)
    check(model.is_file() and model.is_relative_to(root), "GGUF должен находиться в models-dir")
    args.artifacts.mkdir(parents=True, exist_ok=False)
    artifacts = args.artifacts.resolve()
    kaic = Kaic(args.backend.resolve(), args.cli.resolve(), artifacts, args.endpoint,
                footprint=args.footprint_mb, budget=args.budget_mb,
                model_id=args.model_id, models_dir=root, model_path=model)
    try:
        wait_for(kaic.ready, "запуск KAIC", seconds=30)
        # New inventory route inherits existing authentication.
        try:
            HTTP.open(BASE + "/local-models", timeout=3)
            raise AssertionError("каталог доступен без токена")
        except urllib.error.HTTPError as error:
            check(error.code == 401, "неожиданный статус авторизации каталога")
        catalog = kaic.get("/local-models")
        matches = [item for item in catalog["models"] if Path(item["path"]) == model]
        check(len(matches) == 1 and matches[0]["tensor_count"] > 0,
              "GGUF не прошёл discovery или зарегистрирован повторно")
        info = kaic.get("/backend")
        check(info["connected"] is True and info["externally_managed"] is True,
              "внешний backend недоступен")
        check([item["id"] for item in info["models"]] == [args.model_id],
              "сервер обслуживает другую модель")
        for flag, name in [("--local-models", "local-models.txt"),
                           ("--backend-info", "backend-info.txt"),
                           ("--models", "models.txt")]:
            (artifacts / name).write_text(kaic.command(flag))
        task, output = kaic.submit(
            "KAIC (Kirpich AI Core) is a local system coordinating AI models. "
            "Describe it in exactly one short Russian sentence.", seconds=args.timeout)
        (artifacts / "cli-response.txt").write_text(output)
        check(task["category"] == "Simple", "запрос не прошёл ожидаемый Router")
        check(task["status"] == "done", "inference не завершился: " + output)
        answers = [entry["content"] for entry in task["context"]
                   if entry["role"] == "assistant" and entry.get("content")]
        check(answers, "нет непустого ответа assistant")
        check("СИНТЕТИЧЕСКИЙ ОТВЕТ" not in output, "обнаружен synthetic fixture")
        status = kaic.idle()
        check([item["model"] for item in status["loaded_models"]] == ["Local"],
              "Scheduler выбрал другую ресурсную запись")
        (artifacts / "scheduler.txt").write_text(kaic.command("--status"))
        report = {"model": matches[0], "backend": info, "task": task, "scheduler": status}
        (artifacts / "result.json").write_text(json.dumps(report, ensure_ascii=False, indent=2))
    finally:
        kaic.close()
    with sqlite3.connect(f"file:{artifacts / 'kaic.db'}?mode=ro", uri=True) as database:
        row = database.execute("SELECT category, status, context FROM tasks WHERE id = ?",
                               (task["id"],)).fetchone()
    check(row is not None and row[0:2] == ("Simple", "done"), "SQLite не сохранил Done")
    # Task Store persists context as YAML, while its API exposes JSON.
    (artifacts / "sqlite-context.yaml").write_text(row[2])
    check("role: assistant" in row[2] and "content:" in row[2], "SQLite не сохранил ответ")
    print(output.strip())
    print("CLI → Router → Capability Registry → Scheduler → llama-server → SQLite: PASS")
    print("Артефакты:", artifacts)


if __name__ == "__main__":
    main()
