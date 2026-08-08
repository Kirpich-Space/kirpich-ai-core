

import bpy
import os

bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

bpy.ops.mesh.primitive_cube_add(size=2.0, location=(0, 0, 0))

output_dir = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r1"
os.makedirs(output_dir, exist_ok=True)

filepath = os.path.join(output_dir, "task1.blend")
bpy.ops.wm.save_as_mainfile(filepath=filepath)