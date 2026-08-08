

import bpy
for obj in bpy.context.collection.objects.copy():
    if obj.type == 'MESH':
        bpy.data.objects.remove(obj)
bpy.ops.mesh.primitive_cube_add(size=1.0, location=(0, 0, 0))
filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.7_en_r1/task1.blend"
bpy.ops.wm.save_as_mainfile(filepath=filepath)