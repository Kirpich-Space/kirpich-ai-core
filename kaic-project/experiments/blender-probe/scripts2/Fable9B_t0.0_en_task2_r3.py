

import bpy
from pathlib import Path

output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r3/task2.png"
Path(output_path).mkdir(parents=True, exist_ok=True)

bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))
cube = bpy.context.object

mat = bpy.data.materials.new(name="RedMaterial")
mat['base_color'] = (1.0, 0.0, 0.0)
cube.data.materials.append(mat)

bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
bpy.context.scene.frame_start = 1
bpy.context.scene.frame_end = 1
bpy.context.scene.render.filepath = str(output_path)
bpy.context.scene.render.image_settings.file_format = 'PNG'

bpy.ops.render.render(use_viewport=False)