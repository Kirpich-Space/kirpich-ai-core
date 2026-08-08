

import bpy

bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

cube = bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0), rotation=(0, 0, 0))
mat = bpy.data.materials.new(name="RedMaterial")
mat.base_color = (1.0, 0.0, 0.0)
cube.materials.append(mat)

cam = bpy.ops.object.camera_add(location=(0, 0, -2), rotation=(0, 0, 0))
bpy.context.scene.camera = cam

light = bpy.ops.light.add(type='AREA', location=(0, 0, -1.5), size=(4, 4))

render.resolution_x = 320
render.resolution_y = 240
render.resolution_percentage = 100
render.filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r2/task2.png"

bpy.ops.render.render(write_still=True)