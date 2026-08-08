

import bpy

bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))

filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r2/task1.blend"
bpy.ops.wm.save_as_mainfile(filepath=filepath)