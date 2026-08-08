# Проверка задания 2: PNG 320x240, не чёрный, преобладает красный.
# Запуск: blender --background --python check2.py -- <путь к .png>
#
# Читаем пиксели через bpy.data.images — внешних зависимостей не нужно.
# «Красный» определён численно: доля пикселей, где r заметно больше g и b.
import bpy, sys, os

path = sys.argv[sys.argv.index("--") + 1]
fails = []

if not os.path.isfile(path):
    print("FAIL: файла нет:", path)
    sys.exit(1)
if os.path.getsize(path) == 0:
    print("FAIL: файл пустой")
    sys.exit(1)

img = bpy.data.images.load(path)
w, h = img.size
if (w, h) != (320, 240):
    fails.append(f"размер {w}x{h}, а требовалось 320x240")

px = list(img.pixels)
n = len(px) // 4
if n == 0:
    fails.append("в изображении нет пикселей")
else:
    lit = 0      # не чёрные
    reddish = 0  # красные
    for i in range(n):
        r, g, b = px[i * 4], px[i * 4 + 1], px[i * 4 + 2]
        if r + g + b > 0.05:
            lit += 1
        if r > 0.15 and r > g * 1.8 and r > b * 1.8:
            reddish += 1
    if lit / n < 0.01:
        fails.append(f"изображение практически чёрное (не чёрных пикселей {lit/n:.3%})")
    if reddish / n < 0.02:
        fails.append(f"красных пикселей всего {reddish/n:.3%}, требуется не менее 2%")

print("FAIL: " + "; ".join(fails) if fails else "PASS")
sys.exit(1 if fails else 0)
