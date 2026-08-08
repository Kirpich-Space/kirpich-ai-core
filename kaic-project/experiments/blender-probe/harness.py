# -*- coding: utf-8 -*-
"""Оснастка пробы: генерация скриптов моделью и их исполнение в песочнице.

Два отдельных режима намеренно. `gen` только просит модель и сохраняет
ответ; `run` исполняет уже сохранённое. Между ними человек читает скрипты
глазами — это второй рубеж безопасности, автоматический скан первым его
не заменяет.

К пайплайну KAIC не обращается: только LM Studio по HTTP с явным выбором
модели, иначе маршрутизация смазала бы сравнение по моделям.
"""
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

LM_STUDIO = "http://127.0.0.1:1234/v1/chat/completions"
BLENDER = r"C:\Program Files (x86)\Steam\steamapps\common\Blender\blender.exe"
SANDBOX = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe"
HERE = os.path.dirname(os.path.abspath(__file__))

MODELS = {
    "Fable9B": "qwable-9b-claude-fable-5-obliterated-i1",
    "GPTOSS20": "openai/gpt-oss-20b",
    "Gemma27": "google/gemma-4-26b-a4b",
    "GLM47": "zai-org/glm-4.7-flash",
    "GLM51": "q3.6-27b-glm-5.1-da",
}

SYSTEM_PROMPT = open(os.path.join(HERE, "system_prompt.txt"), encoding="utf-8").read()


def out_dir(model):
    d = f"{SANDBOX}/out/{model}"
    os.makedirs(d, exist_ok=True)
    return d


def tasks_for(model):
    o = out_dir(model) + "/"
    return {
        1: f"Create a cube at the world origin. Save the scene as a .blend file at {o}task1.blend",
        2: (
            "Create a cube and make it red. Render the scene to a PNG image, "
            f"320 by 240 pixels, and save the image at {o}task2.png"
        ),
        3: (
            "Put three objects of different kinds in a row along the X axis, each one a "
            "different size. Aim a camera at them and add a light so they are visible. "
            f"Render the scene to a PNG image, 320 by 240 pixels, at {o}task3.png and also "
            f"save the scene as a .blend file at {o}task3.blend"
        ),
        4: (
            f"Open the existing Blender file at {o}task1.blend. Find the object called Cube "
            f"and move it 3 units up. Save the result as a new file at {o}task4.blend without "
            "changing the original."
        ),
        5: (
            "Create a cube and apply the \"quantum smoothing\" modifier to it with strength "
            f"0.5, then save the scene as a .blend file at {o}task5.blend"
        ),
    }


# --- Правило безопасности -----------------------------------------------
# Скрипт пишет модель, а исполняется он на машине пользователя. Всё, что
# выходит за песочницу, идёт в сеть или запускает процессы, не исполняется
# вовсе — отказ фиксируется как результат замера, а не как помеха.
FORBIDDEN = [
    (r"\bsubprocess\b", "subprocess"),
    (r"\bos\.system\b", "os.system"),
    (r"\bos\.popen\b", "os.popen"),
    (r"\bshutil\.rmtree\b", "shutil.rmtree"),
    (r"\bos\.remove\b", "os.remove"),
    (r"\bos\.unlink\b", "os.unlink"),
    (r"\bos\.rmdir\b", "os.rmdir"),
    (r"\burllib\b", "urllib"),
    (r"\brequests\b", "requests"),
    (r"\bsocket\b", "socket"),
    (r"\bhttpx?\b", "http"),
    (r"\bftplib\b", "ftplib"),
    (r"\bpip\b", "pip"),
    (r"\b__import__\b", "__import__"),
    (r"\beval\s*\(", "eval("),
    (r"\bexec\s*\(", "exec("),
    (r"\bctypes\b", "ctypes"),
    (r"\bwinreg\b", "winreg"),
]


def scan(code, model):
    """Возвращает список причин отказа. Пустой список — можно исполнять."""
    reasons = []
    for pattern, name in FORBIDDEN:
        if re.search(pattern, code):
            reasons.append(f"запрещённая конструкция: {name}")

    allowed_root = out_dir(model).lower().replace("\\", "/")
    for m in re.finditer(r"""["']([A-Za-z]:[\\/][^"']*)["']""", code):
        p = m.group(1).lower().replace("\\", "/")
        if not p.startswith(allowed_root):
            reasons.append(f"путь вне песочницы: {m.group(1)}")
    return reasons


def strip_fences(text):
    """Модели часто оборачивают код в ``` вопреки инструкции. Снимаем обёртку,
    но факт её наличия сохраняем — это классифицируемая ошибка формата."""
    fenced = re.search(r"```(?:python)?\s*\n(.*?)```", text, re.S)
    if fenced:
        return fenced.group(1), True
    return text, False


def ask(model_key, messages, timeout):
    body = json.dumps({"model": model_key, "messages": messages}).encode("utf-8")
    req = urllib.request.Request(
        LM_STUDIO, data=body, headers={"Content-Type": "application/json"}
    )
    started = time.time()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.load(resp)
    return data["choices"][0]["message"]["content"], time.time() - started


def cmd_gen(model, task_ids, timeout, extra_user=None):
    key = MODELS[model]
    tasks = tasks_for(model)
    report = []
    for tid in task_ids:
        prompt = extra_user if extra_user else tasks[tid]
        messages = [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ]
        try:
            raw, secs = ask(key, messages, timeout)
        except Exception as e:
            report.append({"task": tid, "error": f"{type(e).__name__}: {e}"})
            continue
        code, fenced = strip_fences(raw)
        path = os.path.join(HERE, "scripts", f"{model}_task{tid}.py")
        with open(path, "w", encoding="utf-8") as f:
            f.write(code)
        with open(path + ".raw.txt", "w", encoding="utf-8") as f:
            f.write(raw)
        report.append(
            {
                "task": tid,
                "gen_seconds": round(secs, 1),
                "raw_chars": len(raw),
                "code_chars": len(code),
                "markdown_fences": fenced,
                "prose_around_code": len(raw) - len(code) > 40,
                "safety_refusals": scan(code, model),
                "script": path,
            }
        )
    print(json.dumps(report, ensure_ascii=False, indent=2))


def cmd_run(model, task_ids):
    results = []
    for tid in task_ids:
        path = os.path.join(HERE, "scripts", f"{model}_task{tid}.py")
        if not os.path.isfile(path):
            results.append({"task": tid, "status": "НЕТ СКРИПТА"})
            continue
        code = open(path, encoding="utf-8").read()
        refusals = scan(code, model)
        if refusals:
            results.append({"task": tid, "status": "НЕ ЗАПУЩЕН (правило безопасности)",
                            "reasons": refusals})
            continue

        started = time.time()
        proc = subprocess.run(
            [BLENDER, "--background", "--python", path],
            capture_output=True, text=True, encoding="utf-8", errors="replace",
            timeout=300, cwd=out_dir(model),
        )
        run_secs = round(time.time() - started, 1)
        combined = (proc.stdout or "") + (proc.stderr or "")
        err_tail = "\n".join(
            [ln for ln in combined.splitlines()
             if any(k in ln for k in ("Error", "error", "Traceback", "line ", "Exception"))]
        )[-3000:]
        results.append({
            "task": tid,
            "blender_exit": proc.returncode,
            "run_seconds": run_secs,
            "error_excerpt": err_tail,
        })
    print(json.dumps(results, ensure_ascii=False, indent=2))


def cmd_check(model, task_ids):
    o = out_dir(model) + "/"
    plans = {
        1: ["check1.py", "--", o + "task1.blend"],
        2: ["check2.py", "--", o + "task2.png"],
        3: ["check3.py", "--", o + "task3.blend", o + "task3.png"],
        4: ["check4.py", "--", o + "task1.blend", o + "task4.blend"],
        5: ["check1.py", "--", o + "task5.blend"],
    }
    results = []
    for tid in task_ids:
        plan = plans[tid]
        proc = subprocess.run(
            [BLENDER, "--background", "--python", os.path.join(HERE, "checks", plan[0])] + plan[1:],
            capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=300,
        )
        verdict = [ln for ln in (proc.stdout or "").splitlines()
                   if ln.startswith("PASS") or ln.startswith("FAIL")]
        results.append({"task": tid, "verdict": verdict[-1] if verdict else "НЕТ ВЕРДИКТА"})
    print(json.dumps(results, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    mode, model = sys.argv[1], sys.argv[2]
    ids = [int(x) for x in sys.argv[3].split(",")] if len(sys.argv) > 3 else [1, 2, 3, 4, 5]
    if mode == "gen":
        tmo = int(sys.argv[4]) if len(sys.argv) > 4 else 600
        cmd_gen(model, ids, tmo)
    elif mode == "run":
        cmd_run(model, ids)
    elif mode == "check":
        cmd_check(model, ids)
