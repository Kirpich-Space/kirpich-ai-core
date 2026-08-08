import bpy
import os

# Delete default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create cube
bpy.ops.mesh.primitive_cube_add(size=2, location=(0, 0, 0))
cube = bpy.context.active_object

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.diffuse_color = (1.0, 0.0, 0.0, 1.0)
cube.data.materials.append(mat)

# Set render settings
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240
bpy.context.scene.render.image_settings.file_format = 'PNG'

output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.0_en_r1/task2.png"
os.makedirs(os.path.dirname(output_path), exist_ok=True)
bpy.context.scene.render.filepath = output_path

# Render and save
bpy.ops.render.render(write_still=True)