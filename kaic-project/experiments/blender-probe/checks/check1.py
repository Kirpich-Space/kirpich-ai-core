# Проверка задания 1: куб в начале координат, сохранён .blend.
# Запуск: blender --background --python check1.py -- <путь к .blend>
#
# Объективность: не «похоже на куб», а число вершин/рёбер/граней куба
# и координаты с допуском.
import bpy, sys, os

path = sys.argv[sys.argv.index("--") + 1]
fails = []

if not os.path.isfile(path):
    print("FAIL: файла нет:", path)
    sys.exit(1)
if os.path.getsize(path) == 0:
    print("FAIL: файл пустой")
    sys.exit(1)

bpy.ops.wm.open_mainfile(filepath=path)

meshes = [o for o in bpy.data.objects if o.type == "MESH"]
if not meshes:
    fails.append("в сцене нет ни одного меша")

cubes = []
for o in meshes:
    m = o.data
    if len(m.vertices) == 8 and len(m.edges) == 12 and len(m.polygons) == 6:
        cubes.append(o)
if not cubes:
    fails.append(
        "нет объекта с топологией куба (8 вершин / 12 рёбер / 6 граней); "
        + "найдено: " + ", ".join(f"{o.name}:{len(o.data.vertices)}в" for o in meshes)
    )
else:
    at_origin = [o for o in cubes if max(abs(c) for c in o.location) < 1e-3]
    if not at_origin:
        fails.append(
            "куб есть, но не в начале координат: "
            + ", ".join(f"{o.name}{tuple(round(c,3) for c in o.location)}" for o in cubes)
        )

print("FAIL: " + "; ".join(fails) if fails else "PASS")
sys.exit(1 if fails else 0)
