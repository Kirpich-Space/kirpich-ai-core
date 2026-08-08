import bpy
# Delete all existing objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)
# Create a cube
mesh = bpy.data.meshes.new('CubeMesh')
cube_obj = bpy.data.objects.new('Cube', mesh)
bpy.context.collection.objects.link(cube_obj)
bpy.context.view_layer.objects.active = cube_obj
bpy.ops.object.editmode_toggle()
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))
bpy.ops.object.editmode_toggle()
# Create red material
mat = bpy.data.materials.new(name='RedMaterial')
mat.diffuse_color = (1.0, 0.0, 0.0, 1.0)
cube_obj.data.materials.append(mat)
# Set render settings
scene = bpy.context.scene
scene.render.image_settings.file_format = 'PNG'
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.resolution_percentage = 100
scene.render.filepath = r'C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.7_en_r2/task2.png'
# Render to file
bpy.ops.render.render(write_still=True)