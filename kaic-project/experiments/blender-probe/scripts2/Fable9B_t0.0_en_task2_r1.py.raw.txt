

import bpy
import os

# Add a cube at origin with size 1.0
cube = bpy.data.objects.new("Cube", bpy.ops.mesh.primitive_cube_add(size=1.0))
bpy.context.collection.objects.link(cube)

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.diffuse_color = (1.0, 0.0, 0.0)
cube.data.materials.append(mat)

# Set render resolution
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240

# Output path
out_path = r"C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.0_en_r1/task2.png"
os.makedirs(os.path.dirname(out_path), exist_ok=True)
bpy.context.scene.render.filepath = out_path

# Render single image
bpy.ops.render.render(write_still=True)