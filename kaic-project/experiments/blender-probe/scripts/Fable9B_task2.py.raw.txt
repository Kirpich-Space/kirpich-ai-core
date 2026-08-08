

import bpy
bpy.ops.object.select_all(action='DELETE')
cube = bpy.ops.mesh.primitive_cube_add(size=1, align='WORLDBASED').handle
cube.location = (0, 0, 0)
mat = bpy.data.materials.new(name="Red")
mat.use_nodes = True
bsdf = mat.node_tree.nodes["Principled BSDF"]
bsdf.inputs[9].default_value = (1.0, 0.0, 0.0, 1.0)
cube_data = cube.data
for i, slot in enumerate(cube_data.materials):
    slot.material = mat

empty = bpy.types.Object(name="Target", type='EMPTY')
empty.location = (2.5, 0, -3.5)
bpy.context.view_layer.objects.link(empty)

cam = bpy.data.cameras.new(name="Camera")
cam_obj = bpy.types.Object(name="CamObj", type='CAMERA', data=cam)
cam_obj.location = (-6.0, 0, 2.0)
bpy.context.view_layer.objects.link(cam_obj)
cam_obj.data.target = empty

scene = bpy.context.scene
scene.camera = cam_obj
render = scene.render
render.resolution_x = 320
render.resolution_y = 240
render.resolution_percentage = 100
render.film_transparent = False
render.image_settings.file_format = 'PNG'
render.film_resx = 320
render.film_resy = 240

output = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/Fable9B/task2.png"
import os
os.makedirs(os.path.dirname(output), exist_ok=True)
scene.render.filepath = output