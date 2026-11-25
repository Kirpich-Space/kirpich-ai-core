#!/usr/bin/env python3
"""Процессная проверка CLI → KAIC → локальный дубль llama-server, без GGUF/GPU.

Запуск: python3 tests/cli_inference.py /absolute/path/kaic /absolute/path/kaic-cli
Нужен свободный 127.0.0.1:4545 (существующий main KAIC фиксирует этот адрес).
Занятый порт вызывает отказ; чужие процессы не останавливаются.
"""

import argparse
import contextlib
import http.server
import json
import os
from pathlib import Path
import re
import secrets
import signal
import socket
import subprocess
import tempfile
import threading
import time
import urllib.error
import urllib.request


PROMPT = "Respond with exactly one short Russian sentence explaining what KAIC is."
ANSWER = "СИНТЕТИЧЕСКИЙ ОТВЕТ: KAIC координирует локальные модели."
MODEL = "synthetic-model"
BASE = "http://127.0.0.1:4545"
HTTP = urllib.request.build_opener(urllib.request.ProxyHandler({}))


def check(condition, message):
    if not condition:
        raise AssertionError(message)


def wait_for(probe, description, seconds=12):
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        result = probe()
        if result:
            return result
        time.sleep(0.025)
    raise AssertionError(f"Истекло ожидание: {description}")


class Provider(http.server.ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self):
        super().__init__(("127.0.0.1", 0), Handler)
        self.mode = "normal"
        self.requests = []
        self.lock = threading.Lock()

    def posts(self):
        with self.lock:
            return [request for request in self.requests if request[0] == "POST"]


class Handler(http.server.BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_GET(self):
        self.respond()

    def do_POST(self):
        self.respond()

    def respond(self):
        size = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(size)) if size else None
        with self.server.lock:
            mode = self.server.mode
            self.server.requests.append((self.command, self.path, body))
        code = 200
        if self.path == "/health":
            if mode == "timeout":
                time.sleep(2.5)  # Production diagnostic deadline is two seconds.
            code = 503 if mode == "unavailable" else 200
            value = {"status": "loading" if code == 503 else "ok"}
        elif self.path == "/v1/models":
            value = {"data": [{"id": "another-model" if mode == "missing" else MODEL}]}
        elif self.path == "/v1/chat/completions?autoload=false":
            if mode == "invalid":
                value = {"choices": []}
            elif mode == "null":
                value = {"choices": [{"message": {"content": None}}]}
            elif mode == "http-error":
                code, value = 503, {"error": "synthetic generation failure"}
            else:
                value = {"choices": [{"message": {"content": ANSWER}}]}
        else:
            code, value = 404, {"error": "unexpected endpoint"}
        data = json.dumps(value, ensure_ascii=False).encode()
        try:
            self.send_response(code)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)
        except (BrokenPipeError, ConnectionResetError):
            pass  # Expected when the client enforces its diagnostic timeout.


@contextlib.contextmanager
def provider():
    server = Provider()
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)
        check(not thread.is_alive(), "дубль inference не завершился")


class Kaic:
    def __init__(self, backend, cli, directory, endpoint, footprint=60, mismatch=False,
                 model_id=MODEL, budget=100, models_dir=None, model_path=None):
        self.directory = directory
        self.cli = cli
        self.env = os.environ.copy()
        # Do not inherit an inference token, proxy, model preference or backend
        # selection. All runtime files belong to the temporary working tree.
        for key in list(self.env):
            if key.startswith("KAIC_"):
                del self.env[key]
        self.env.update(
            KAIC_CONTROL_TOKEN=secrets.token_hex(24),
            KAIC_MODEL_BACKEND="llama-server",
            KAIC_INFERENCE_URL=endpoint,
            KAIC_RESOURCE_REGISTRY=str(directory / "resources.yaml"),
            KAIC_CAPABILITY_REGISTRY=str(directory / "capabilities.yaml"),
            KAIC_MODEL_MEMORY_MB=str(budget),
            KAIC_MODELS_DIR=str(models_dir if models_dir is not None else directory / "models"),
            KAIC_API_BASE=BASE,
            RUST_LOG="info",
            NO_COLOR="1",
        )
        entry = {"model_id": model_id, "vram_mb": footprint, "load_seconds": 0, "always_loaded": True}
        if model_path is not None:
            entry["path"] = str(model_path)
        resources = {"Local": entry}
        if mismatch:
            resources["Unavailable"] = {"model_id": "not-served", "vram_mb": 60, "load_seconds": 0}
        (directory / "resources.yaml").write_text(json.dumps(resources, ensure_ascii=False))
        (directory / "capabilities.yaml").write_text(
            f"Simple: {{primary: {'Unavailable' if mismatch else 'Local'}}}\n"
            "Programming: {primary: Local}\n"
        )
        self.log = (directory / "backend.log").open("w+")
        self.process = subprocess.Popen(
            [str(backend)], cwd=directory, env=self.env,
            stdout=self.log, stderr=subprocess.STDOUT,
        )

    def get(self, path):
        request = urllib.request.Request(
            BASE + path, headers={"Authorization": "Bearer " + self.env["KAIC_CONTROL_TOKEN"]}
        )
        with HTTP.open(request, timeout=3) as response:
            return json.load(response)

    def ready(self):
        check(self.process.poll() is None, "KAIC завершился при старте; см. backend.log")
        try:
            return self.get("/status")
        except (OSError, urllib.error.URLError):
            return None

    def command(self, *args, code=0):
        result = subprocess.run(
            [str(self.cli), "--backend", BASE, *args], cwd=self.directory,
            env=self.env, text=True, capture_output=True, timeout=12,
        )
        check(result.returncode == code,
              f"CLI {args}: exit {result.returncode}, expected {code}: {result.stderr}")
        return result.stdout

    def submit(self, prompt=PROMPT, seconds=12):
        output = self.command("--send", prompt)
        match = re.search(r"Задача ([0-9a-f-]{36})", output)
        check(match is not None, f"CLI не вывел UUID: {output}")
        task_id = match[1]

        def terminal():
            task = self.get("/tasks/" + task_id)
            return task if task["status"] in ("done", "failed") else None

        task = wait_for(terminal, "завершение задачи", seconds=seconds)
        return task, self.command("--show", task_id)

    def idle(self):
        def idle_status():
            status = self.get("/status")
            return status if status["running_tasks"] == 0 and not status["busy_models"] else None

        status = wait_for(idle_status, "освобождение Scheduler")
        check(status["active_operation"] is None, "осталась активная lifecycle-операция")
        check(not status["recovery_warnings"], "осталась неопределённая операция")
        return status

    def close(self):
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGINT)
        try:
            self.process.wait(timeout=8)
        except subprocess.TimeoutExpired:
            self.process.kill()
            self.process.wait(timeout=3)
            raise AssertionError("KAIC потребовал SIGKILL при завершении")
        finally:
            self.log.close()


def run_case(backend, cli, root, mode):
    directory = root / mode
    directory.mkdir()
    with provider() as mock:
        kaic = Kaic(
            backend, cli, directory, f"http://127.0.0.1:{mock.server_port}",
            footprint=101 if mode == "budget" else 60, mismatch=mode == "mismatch",
        )
        try:
            wait_for(kaic.ready, "API KAIC")
            info = kaic.command("--backend-info")
            check("llama-server" in info and MODEL in info, "CLI discovery не показал модель")
            mock.mode = mode
            started = time.monotonic()
            task, output = kaic.submit()
            check(task["category"] == "Simple", "Router не выбрал Simple")
            expected = "done" if mode == "normal" else "failed"
            check(task["status"] == expected,
                  f"{mode}: status={task['status']}, expected {expected}; CLI={output}")
            status = kaic.idle()
            posts = mock.posts()
            if mode in ("normal", "invalid", "null", "http-error"):
                check(len(posts) == 1, "генерация должна отправляться ровно один раз")
                _, path, payload = posts[0]
                check(path.endswith("?autoload=false"), "пропал запрет autoload")
                check(payload["model"] == MODEL, "модель провайдера подменена ярлыком")
                check(any(m["role"] == "user" and m["content"] == PROMPT for m in payload["messages"]),
                      "исходный запрос не прошёл CLI/Router/Scheduler")
                check("tools" not in payload, "без MCP не должно быть инструментов")
            else:
                check(not posts, "при отказе до генерации отправлен POST inference")
            if mode == "normal":
                check(ANSWER in output and "готово" in output, "CLI не вывел ответ")
                check(status["used_model_memory_mb"] == 60, "не сохранена внешняя резидентность")
                check([m["model"] for m in status["loaded_models"]] == ["Local"],
                      "не выбран зарегистрированный ярлык Local")
            else:
                check("ошибка" in output, "ошибка задачи не видна в CLI")
                check(any(e["role"] == "system" for e in task["context"]), "не сохранена диагностика")
            if mode in ("unavailable", "timeout"):
                kaic.command("--backend-info", code=1)
            if mode == "timeout":
                check(time.monotonic() - started < 9, "diagnostic timeout не ограничил вызов")
            if mode not in ("normal", "budget"):
                mock.mode = "normal"
                retry_prompt = "напиши код: верни одно слово" if mode == "mismatch" else PROMPT
                recovered, recovered_output = kaic.submit(retry_prompt)
                check(recovered["status"] == "done" and ANSWER in recovered_output,
                      "новая задача после ошибки не восстановила исполнение")
                if mode == "mismatch":
                    check(recovered["category"] == "Programming", "не проверен другой маршрут Router")
                kaic.idle()
            logs = (directory / "backend.log").read_text()
            check("(определена Router'ом)" in logs, "отсутствует реальный вызов Router")
            (directory / "result.json").write_text(json.dumps(
                {"mode": mode, "task": task, "cli_output": output,
                 "scheduler_after_task": status, "inference_posts": posts,
                 "real_gguf_inference": False},
                ensure_ascii=False, indent=2,
            ))
        finally:
            kaic.close()
        print(f"PASS {mode}: реальный CLI/KAIC, синтетический inference", flush=True)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("backend", type=Path)
    parser.add_argument("cli", type=Path)
    parser.add_argument("--artifacts", type=Path, help="новый каталог вне Git для журналов/БД")
    parser.add_argument("--case", choices=("normal", "invalid", "null", "http-error", "unavailable", "timeout", "missing", "budget", "mismatch"))
    args = parser.parse_args()
    backend, cli = args.backend.resolve(strict=True), args.cli.resolve(strict=True)
    with socket.socket() as probe:
        # Match normal listener restart semantics; TIME_WAIT is not an
        # unrelated listener. SO_REUSEPORT is intentionally never enabled.
        probe.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        probe.bind(("127.0.0.1", 4545))
    cases = [args.case] if args.case else ["normal", "invalid", "null", "http-error", "unavailable", "timeout", "missing", "budget", "mismatch"]
    with contextlib.ExitStack() as stack:
        if args.artifacts:
            args.artifacts.mkdir(parents=True, exist_ok=False)
            root = args.artifacts.resolve()
        else:
            root = Path(stack.enter_context(tempfile.TemporaryDirectory(prefix="kaic-cli-inference-")))
        for case in cases:
            run_case(backend, cli, root, case)
    print(f"Итого: {len(cases)} проверок; реальный GGUF inference НЕ выполнялся.")


if __name__ == "__main__":
    main()
