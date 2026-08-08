import bpy
# Delete all default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)
# Create a cube
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 1))
cube = bpy.context.active_object
# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.diffuse_color = (1.0, 0.0, 0.0, 1.0)
cube.data.materials.append(mat)
# Add a camera
cam_data = bpy.data.cameras.new(name='Camera')
cam_obj = bpy.data.objects.new('Camera', cam_data)
bpy.context.collection.objects.link(cam_obj)
cam_obj.location = (5, -5, 5)
cam_obj.rotation_euler = (1.1093, 0.0108, 0.7854)  # roughly looking at origin
bpy.context.scene.camera = cam_obj
# Set render resolution
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240
# Set output path
scene.render.filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task2.png"
# Render and save image
bpy.ops.render.render(write_still=True)