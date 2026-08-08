

import bpy

for obj in bpy.data.objects:
    if obj.name != "__collection_root__":
        bpy.data.objects.remove(obj, do_unlink=False)

bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r3/task1.blend"
bpy.ops.wm.save_as_mainfile(filepath=filepath, check_existing=True)