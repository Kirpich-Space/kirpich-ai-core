# -*- coding: utf-8 -*-
"""Добивка пробы: температура, русский язык, недоизмеренное.

Сравнимость — главное правило. Системный промпт, тексты заданий и программы
проверки берутся из первой пробы БЕЗ изменений: `harness` импортируется, а не
копируется, чтобы расхождение было невозможно физически. Здесь добавляются
только три вещи, которых в первой пробе не было: явная температура, повторы
и русский вариант текста задания.

Результаты пишутся в scripts2/ и out2/ — базовая линия не перезаписывается.
"""
import json
import os
import re
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from harness import BLENDER, HERE, LM_STUDIO, MODELS, SANDBOX, SYSTEM_PROMPT, FORBIDDEN

SCRIPTS2 = os.path.join(HERE, "scripts2")
os.makedirs(SCRIPTS2, exist_ok=True)


def cell_dir(model, temp, lang, rep):
    d = f"{SANDBOX}/out2/{model}_t{temp}_{lang}_r{rep}"
    os.makedirs(d, exist_ok=True)
    return d


# --- Тексты заданий -------------------------------------------------------
# EN — дословно из первой пробы (tasks.md), меняется только путь.
# RU — БУКВАЛЬНЫЙ перевод: переводится язык, а не формулировка требования.
def task_text(tid, out, lang):
    o = out + "/"
    en = {
        1: f"Create a cube at the world origin. Save the scene as a .blend file at {o}task1.blend",
        2: ("Create a cube and make it red. Render the scene to a PNG image, "
            f"320 by 240 pixels, and save the image at {o}task2.png"),
        3: ("Put three objects of different kinds in a row along the X axis, each one a "
            "different size. Aim a camera at them and add a light so they are visible. "
            f"Render the scene to a PNG image, 320 by 240 pixels, at {o}task3.png and also "
            f"save the scene as a .blend file at {o}task3.blend"),
        4: (f"Open the existing Blender file at {o}task1.blend. Find the object called Cube "
            f"and move it 3 units up. Save the result as a new file at {o}task4.blend without "
            "changing the original."),
        5: ("Create a cube and apply the \"quantum smoothing\" modifier to it with strength "
            f"0.5, then save the scene as a .blend file at {o}task5.blend"),
    }
    ru = {
        1: f"Создай куб в начале координат. Сохрани сцену как файл .blend по пути {o}task1.blend",
        2: ("Создай куб и сделай его красным. Отрендери сцену в изображение PNG "
            f"размером 320 на 240 пикселей и сохрани изображение по пути {o}task2.png"),
        3: ("Помести три объекта разных видов в ряд вдоль оси X, каждый разного размера. "
            "Наведи на них камеру и добавь свет, чтобы они были видны. Отрендери сцену в "
            f"изображение PNG размером 320 на 240 пикселей по пути {o}task3.png, а также "
            f"сохрани сцену как файл .blend по пути {o}task3.blend"),
        4: (f"Открой существующий файл Blender по пути {o}task1.blend. Найди объект с именем "
            f"Cube и подними его на 3 единицы вверх. Сохрани результат как новый файл по пути "
            f"{o}task4.blend, не изменяя исходный."),
        5: ("Создай куб и примени к нему модификатор \"quantum smoothing\" с силой 0.5, "
            f"затем сохрани сцену как файл .blend по пути {o}task5.blend"),
    }
    return (en if lang == "en" else ru)[tid]


def scan(code, allowed_root):
    """То же правило безопасности, что и в первой пробе, но с явным корнем."""
    reasons = []
    for pattern, name in FORBIDDEN:
        if re.search(pattern, code):
            reasons.append(f"запрещённая конструкция: {name}")
    root = allowed_root.lower().replace("\\", "/")
    for m in re.finditer(r"""["']([A-Za-z]:[\\/][^"']*)["']""", code):
        p = m.group(1).lower().replace("\\", "/")
        if not p.startswith(root):
            reasons.append(f"путь вне песочницы: {m.group(1)}")
    return reasons


def strip_fences(text):
    fenced = re.search(r"```(?:python)?\s*\n(.*?)```", text, re.S)
    return (fenced.group(1), True) if fenced else (text, False)


def ask(model_key, prompt, temperature, timeout):
    body = json.dumps({
        "model": model_key,
        "messages": [
            {"role": "system", "content": SYSTEM_PROMPT},
            {"role": "user", "content": prompt},
        ],
        "temperature": temperature,
    }).encode("utf-8")
    req = urllib.request.Request(LM_STUDIO, data=body,
                                 headers={"Content-Type": "application/json"})
    t0 = time.time()
    with urllib.request.urlopen(req, timeout=timeout) as resp:
        data = json.load(resp)
    return data["choices"][0]["message"]["content"], round(time.time() - t0, 1)


CHECK_PLAN = {
    1: ("check1.py", ["task1.blend"]),
    2: ("check2.py", ["task2.png"]),
    3: ("check3.py", ["task3.blend", "task3.png"]),
    4: ("check4.py", ["task1.blend", "task4.blend"]),
    5: ("check1.py", ["task5.blend"]),
}


def gen_cell(model, tid, temp, lang, rep, timeout):
    """ТОЛЬКО генерация. Исполнения здесь нет намеренно: между генерацией и
    запуском скрипт читает человек — это второй рубеж безопасности, и
    автоматический скан его не заменяет."""
    out = cell_dir(model, temp, lang, rep)
    tag = f"{model}_t{temp}_{lang}_task{tid}_r{rep}"
    prompt = task_text(tid, out, lang)

    try:
        raw, gen_s = ask(MODELS[model], prompt, temp, timeout)
    except Exception as e:
        return {"cell": tag, "outcome": f"ОШИБКА ЗАПРОСА: {type(e).__name__}", "gen_s": None}

    code, fenced = strip_fences(raw)
    path = os.path.join(SCRIPTS2, tag + ".py")
    open(path, "w", encoding="utf-8").write(code)
    open(path + ".raw.txt", "w", encoding="utf-8").write(raw)

    return {"cell": tag, "gen_s": gen_s, "chars": len(code), "fenced": fenced,
            "refusals": scan(code, out), "empty": not code.strip()}


def run_cell(model, tid, temp, lang, rep):
    """ТОЛЬКО исполнение уже сгенерированного и прочитанного скрипта."""
    out = cell_dir(model, temp, lang, rep)
    tag = f"{model}_t{temp}_{lang}_task{tid}_r{rep}"
    path = os.path.join(SCRIPTS2, tag + ".py")
    if not os.path.isfile(path):
        return {"cell": tag, "outcome": "НЕТ СКРИПТА"}
    code = open(path, encoding="utf-8").read()

    refusals = scan(code, out)
    if refusals:
        return {"cell": tag, "outcome": "НЕ ЗАПУЩЕН (безопасность)", "reasons": refusals}
    if not code.strip():
        return {"cell": tag, "outcome": "ПУСТОЙ ОТВЕТ"}

    t0 = time.time()
    proc = subprocess.run([BLENDER, "--background", "--python", path],
                          capture_output=True, text=True, encoding="utf-8",
                          errors="replace", timeout=400, cwd=out)
    run_s = round(time.time() - t0, 1)
    combined = (proc.stdout or "") + (proc.stderr or "")
    errs = [ln for ln in combined.splitlines()
            if any(k in ln for k in ("Error", "error", "Traceback", "Exception"))]

    chk_file, chk_args = CHECK_PLAN[tid]
    chk = subprocess.run(
        [BLENDER, "--background", "--python", os.path.join(HERE, "checks", chk_file), "--"]
        + [out + "/" + a for a in chk_args],
        capture_output=True, text=True, encoding="utf-8", errors="replace", timeout=400)
    verdict = [ln for ln in (chk.stdout or "").splitlines()
               if ln.startswith("PASS") or ln.startswith("FAIL")]

    return {
        "cell": tag, "run_s": run_s,
        "blender_error": (errs[-1][:120] if errs else ""),
        "outcome": (verdict[-1][:120] if verdict else "НЕТ ВЕРДИКТА"),
    }


if __name__ == "__main__":
    mode = sys.argv[1]                       # gen | run
    model = sys.argv[2]
    tids = [int(x) for x in sys.argv[3].split(",")]
    temps = [float(x) for x in sys.argv[4].split(",")]
    reps = int(sys.argv[5])
    lang = sys.argv[6] if len(sys.argv) > 6 else "en"
    timeout = int(sys.argv[7]) if len(sys.argv) > 7 else 900

    results = []
    for temp in temps:
        for tid in tids:
            for rep in range(1, reps + 1):
                r = (gen_cell(model, tid, temp, lang, rep, timeout) if mode == "gen"
                     else run_cell(model, tid, temp, lang, rep))
                results.append(r)
                print(json.dumps(r, ensure_ascii=False), flush=True)
    open(os.path.join(HERE, f"results2_{mode}_{model}_{lang}.json"), "a",
         encoding="utf-8").write(json.dumps(results, ensure_ascii=False, indent=2) + "\n")
