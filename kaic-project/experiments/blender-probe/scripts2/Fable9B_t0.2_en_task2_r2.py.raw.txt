

import bpy
import os

# Clean up existing objects
for obj in bpy.data.objects:
    bpy.ops.object.delete(use_collection=False)

# Add a cube
cube = bpy.ops.mesh.primitive_cube_add(size=1, location=(0, 0, 0)).object

# Create red material
mat = bpy.data.materials.new(name="RedMaterial")
mat.use_nodes = True
nodes = mat.node_tree.nodes
links = mat.node_tree.links
output_node = nodes.get("Output")
if not output_node:
    output_node = nodes.new(type="ShaderNodeOutputWorld")
principled = nodes.get("Principled BSDF")
if not principled:
    principled = nodes.new(type="ShaderNodeBsdfPrincipled")
links.new(principled.outputs[0], output_node.inputs[0])
principled.inputs['Base Color'].default_value = (1.0, 0.0, 0.0, 1.0)

cube.data.materials.append(mat)

# Ensure a camera exists
if not any(obj.type == 'CAMERA' for obj in bpy.data.objects):
    bpy.ops.object.camera_add(location=(0, -2, 1))

# Set render resolution
bpy.context.scene.render.resolution_x = 320
bpy.context.scene.render.resolution_y = 240

# Render to the specified path
target_path = "C:/Users/KIRPIC~1/AppData/Local/Temp/blender-probe/out2/Fable9B_t0.2_en_r2/task2.png"
os.makedirs(os.path.dirname(target_path), exist_ok=True)
bpy.context.scene.render.filepath = target_path
bpy.ops.render.render(use_viewport=False)