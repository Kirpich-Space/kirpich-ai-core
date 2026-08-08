# Проверка задания 3: три разных объекта в ряд, разного размера,
# камера, свет, непустой рендер.
# Запуск: blender --background --python check3.py -- <.blend> <.png>
#
# «Разные виды объектов» проверяются числом вершин: у примитивов Blender
# оно разное (куб 8, конус 33, цилиндр 64, UV-сфера 482, тор 576, обезьяна 507).
# Три разных числа вершин — объективный признак трёх разных примитивов.
import bpy, sys, os

args = sys.argv[sys.argv.index("--") + 1:]
blend_path, png_path = args[0], args[1]
fails = []

for p in (blend_path, png_path):
    if not os.path.isfile(p):
        print("FAIL: файла нет:", p)
        sys.exit(1)
    if os.path.getsize(p) == 0:
        print("FAIL: файл пустой:", p)
        sys.exit(1)

# --- рендер ---
img = bpy.data.images.load(png_path)
w, h = img.size
if (w, h) != (320, 240):
    fails.append(f"рендер {w}x{h}, а требовалось 320x240")
px = list(img.pixels)
n = len(px) // 4
lit = sum(1 for i in range(n) if px[i * 4] + px[i * 4 + 1] + px[i * 4 + 2] > 0.05)
if n == 0 or lit / n < 0.01:
    fails.append("рендер практически чёрный — сцена не освещена или камера не туда")

# --- сцена ---
bpy.ops.wm.open_mainfile(filepath=blend_path)

cameras = [o for o in bpy.data.objects if o.type == "CAMERA"]
lights = [o for o in bpy.data.objects if o.type == "LIGHT"]
meshes = [o for o in bpy.data.objects if o.type == "MESH"]

if not cameras:
    fails.append("в сцене нет камеры")
if not lights:
    fails.append("в сцене нет источника света")
if len(meshes) < 3:
    fails.append(f"мешей всего {len(meshes)}, требуется не менее 3")
else:
    vert_counts = {len(o.data.vertices) for o in meshes}
    if len(vert_counts) < 3:
        fails.append(
            f"объекты не трёх разных видов: наборы вершин {sorted(vert_counts)}"
        )
    xs = [round(o.location.x, 3) for o in meshes]
    if len(set(xs)) < 3:
        fails.append(f"объекты не расставлены в ряд по X: координаты {xs}")
    sizes = [round(max(o.dimensions), 3) for o in meshes]
    if len(set(sizes)) < 3:
        fails.append(f"размеры не все разные: габариты {sizes}")

print("FAIL: " + "; ".join(fails) if fails else "PASS")
sys.exit(1 if fails else 0)
