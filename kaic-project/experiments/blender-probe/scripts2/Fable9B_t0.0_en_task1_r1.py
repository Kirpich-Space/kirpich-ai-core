

import bpy
bpy.ops.mesh.primitive_cube_add(location=(0, 0, 0))
obj = bpy.context.selected_objects[0]
bpy.context.view_layer.objects.active = obj
import os
os.makedirs(r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r1", exist_ok=True)
bpy.ops.wm.save_as_mainfile(filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r1/task1.blend")