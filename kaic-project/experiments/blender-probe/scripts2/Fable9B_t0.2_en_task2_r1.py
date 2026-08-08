

import bpy

bpy.ops.wm.read_factory_settings(use_empty=True)
bpy.context.collection.objects.unlink(*[obj for obj in bpy.context.collection.objects])

cube = bpy.data.meshes.new("Cube")
cube.from_pydata(
    vertices=[(0, 0, 0), (1, 0, 0), (1, 1, 0), (0, 1, 0),
              (0, 0, 1), (1, 0, 1), (1, 1, 1), (0, 1, 1)],
    edges=[(0, 1), (1, 2), (2, 3), (3, 0),
           (4, 5), (5, 6), (6, 7), (7, 4),
           (0, 4), (1, 5), (2, 6), (3, 7)],
    faces=[(3, 2, 1, 0), (7, 6, 5, 4), (3, 7, 6, 2), (0, 4, 7, 3)]
)
cube.calc_all()

mat = bpy.data.materials.new("RedMat")
mat.use_nodes = True
bsdf = mat.node_tree.nodes["Principled BSDF"]
bsdf.inputs[9].default_value = (1.0, 0.0, 0.0)
cube.materials.append(mat)

bpy.ops.object.mesh_add(type="CUBE")
obj = bpy.context.active_object
obj.data = cube
obj.name = "Cube"
obj.location = (0.5, 0, 0)

cam = bpy.data.cameras.new("Cam")
bpy.ops.object.camera_add(location=(3, 0, 3))
cam_obj = bpy.context.active_object
cam_obj.data = cam
cam_obj.rotation_euler = (0, 0, 0)
cam_obj.location = (3, 0, 3)

light = bpy.data.lamps.new("Light")
bpy.ops.object.light_add(type="AREA", location=(0, -2, 1))
light_obj = bpy.context.active_object
light_obj.data = light

bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
bpy.context.window_manager.save_as_rendered(
    filepath=r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r1/task2.png"
)