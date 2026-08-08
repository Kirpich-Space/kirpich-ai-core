import bpy
# delete default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)
# create first object: cube size 1 at x=-2
bpy.ops.mesh.primitive_cube_add(size=1, location=(-2,0,0))
cube = bpy.context.active_object
# second object: UV sphere radius 1.5 at x=0
bpy.ops.mesh.primitive_uv_sphere_add(radius=1.5, location=(0,0,0))
sphere = bpy.context.active_object
# third object: cone height 3 base_radius 1 at x=2
bpy.ops.mesh.primitive_cone_add(vertices=32, radius1=1, depth=3, location=(2,0,0))
cone = bpy.context.active_object
# set up camera
cam_data = bpy.data.cameras.new('Camera')
cam_obj = bpy.data.objects.new('Camera', cam_data)
bpy.context.collection.objects.link(cam_obj)
cam_obj.location = (0, -10, 5)
cam_obj.rotation_euler = (1.2, 0, 0)
bpy.context.scene.camera = cam_obj
# set up light
light_data = bpy.data.lights.new(name='Light', type='POINT')
light_obj = bpy.data.objects.new('Light', light_data)
bpy.context.collection.objects.link(light_obj)
light_obj.location = (0, -5, 10)
# render settings
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.resolution_percentage = 100
scene.render.image_settings.file_format = 'PNG'
output_path = r'C:\Users\KIRPIC~1\AppData\Local\Temp\blender-probe\out\GPTOSS20\task3.png'
scene.render.filepath = output_path
bpy.ops.render.render(write_still=True)
# save blend file
blend_path = r'C:\Users\KIRPIC~1\AppData\Local\Temp\blender-probe\out\GPTOSS20\task3.blend'
bpy.ops.wm.save_mainfile(filepath=blend_path)