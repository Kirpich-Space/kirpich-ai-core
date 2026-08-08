# Проверка задания 4: объект Cube поднят на 3 по Z, исходный файл не тронут.
# Запуск: blender --background --python check4.py -- <исходный .blend> <новый .blend>
import bpy, sys, os

args = sys.argv[sys.argv.index("--") + 1:]
src_path, dst_path = args[0], args[1]
fails = []

if not os.path.isfile(dst_path):
    print("FAIL: нового файла нет:", dst_path)
    sys.exit(1)
if os.path.getsize(dst_path) == 0:
    print("FAIL: новый файл пустой")
    sys.exit(1)
if os.path.abspath(src_path) == os.path.abspath(dst_path):
    print("FAIL: исходный и новый файл — один и тот же путь")
    sys.exit(1)

# Исходный: где Cube был.
bpy.ops.wm.open_mainfile(filepath=src_path)
src = bpy.data.objects.get("Cube")
if src is None:
    print("FAIL: в исходном файле нет объекта Cube — задание невыполнимо, проверка недействительна")
    sys.exit(2)
src_z = src.location.z

# Новый: где Cube стал.
bpy.ops.wm.open_mainfile(filepath=dst_path)
dst = bpy.data.objects.get("Cube")
if dst is None:
    fails.append("в новом файле нет объекта с именем Cube")
else:
    moved = dst.location.z - src_z
    if abs(moved - 3.0) > 1e-3:
        fails.append(f"Cube сместился по Z на {moved:.3f}, а требовалось ровно 3")
    if abs(dst.location.x - src.location.x) > 1e-3 or abs(dst.location.y - src.location.y) > 1e-3:
        fails.append("сместился не только по Z")

print("FAIL: " + "; ".join(fails) if fails else "PASS")
sys.exit(1 if fails else 0)
