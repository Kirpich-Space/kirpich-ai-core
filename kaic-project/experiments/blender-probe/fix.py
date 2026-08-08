# -*- coding: utf-8 -*-
"""Цикл починки: вернуть модели её скрипт и дословную ошибку Blender.

Предел — три попытки на задание, включая первую. Промпт починки один и тот
же для всех моделей и зафиксирован в tasks.md; здесь он не варьируется.
"""
import json
import os
import subprocess
import sys
import time

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import (BLENDER, HERE, MODELS, SYSTEM_PROMPT, ask, out_dir, scan,
                     strip_fences, tasks_for)

FIX_TEMPLATE = (
    "This script failed. Here is the error Blender printed:\n\n{error}\n\n"
    "Fix the script. Reply with the corrected script only."
)


def blender_error(path, model):
    proc = subprocess.run(
        [BLENDER, "--background", "--python", path],
        capture_output=True, text=True, encoding="utf-8", errors="replace",
        timeout=300, cwd=out_dir(model),
    )
    combined = (proc.stdout or "") + (proc.stderr or "")
    lines = combined.splitlines()
    keep = [ln for ln in lines
            if any(k in ln for k in ("Traceback", "Error", "error", "line ", "Exception"))]
    return "\n".join(keep[:40])


def main(model, tid, max_attempts, timeout):
    key = MODELS[model]
    task_text = tasks_for(model)[tid]
    script = os.path.join(HERE, "scripts", f"{model}_task{tid}.py")
    log = []

    for attempt in range(2, max_attempts + 1):
        err = blender_error(script, model)
        if not err.strip():
            log.append({"attempt": attempt, "note": "ошибок нет — чинить нечего"})
            break

        messages = [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": task_text},
            {"role": "assistant", "content": open(script, encoding="utf-8").read()},
            {"role": "user", "content": FIX_TEMPLATE.format(error=err)},
        ]
        try:
            raw, secs = ask(key, messages, timeout)
        except Exception as e:
            log.append({"attempt": attempt, "error": f"{type(e).__name__}: {e}"})
            break

        code, fenced = strip_fences(raw)
        script = os.path.join(HERE, "scripts", f"{model}_task{tid}_attempt{attempt}.py")
        with open(script, "w", encoding="utf-8") as f:
            f.write(code)
        log.append({
            "attempt": attempt,
            "gen_seconds": round(secs, 1),
            "code_chars": len(code),
            "markdown_fences": fenced,
            "safety_refusals": scan(code, model),
            "error_given_back": err.splitlines()[-1][:160] if err.splitlines() else "",
            "script": os.path.basename(script),
        })
    print(json.dumps(log, ensure_ascii=False, indent=2))


if __name__ == "__main__":
    main(sys.argv[1], int(sys.argv[2]),
         int(sys.argv[3]) if len(sys.argv) > 3 else 3,
         int(sys.argv[4]) if len(sys.argv) > 4 else 600)
