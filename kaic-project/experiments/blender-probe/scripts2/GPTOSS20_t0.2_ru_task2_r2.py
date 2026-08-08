import bpy, os

# Remove default objects
bpy.ops.object.select_all(action='SELECT')
bpy.ops.object.delete(use_global=False)

# Create a cube
bpy.ops.mesh.primitive_cube_add(size=2)
cube = bpy.context.active_object

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.diffuse_color = (1.0, 0.0, 0.0, 1.0)  # RGBA
cube.data.materials.append(mat)

# Set render resolution
scene = bpy.context.scene
scene.render.resolution_x = 320
scene.render.resolution_y = 240
scene.render.resolution_percentage = 100

# Output path
output_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/GPTOSS20_t0.2_ru_r2/task2.png"
os.makedirs(os.path.dirname(output_path), exist_ok=True)
scene.render.filepath = output_path

# Render and save image
bpy.ops.render.render(write_still=True)