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
# Set render resolution
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
# Set output path
bpy.context.scene.render.filepath = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out/GPTOSS20/task2.png"
# Render and save image
bpy.ops.render.render(write_still=True)